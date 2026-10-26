use terrio_crs::{SpatialRef, Transformer};

#[test]
fn pinned_tilted_perspective_fixture_and_backside_refusal() {
    let data: serde_json::Value = serde_json::from_str(include_str!("data/perspective_benchmark.json")).unwrap();
    assert_eq!(data["environment"]["proj_version"], "9.9.0");
    for case in data["fixtures"].as_array().unwrap() {
        let source = SpatialRef::parse(case["src"].as_str().unwrap()).unwrap();
        let target = SpatialRef::parse(case["dst"].as_str().unwrap()).unwrap();
        let x = case["point"][0].as_f64().unwrap();
        let y = case["point"][1].as_f64().unwrap();
        let actual = Transformer::new(&source, &target).unwrap().transform(x, y);
        if case["oracle"]["executed"] == false {
            assert!(actual.is_err());
            continue;
        }
        let actual = actual.unwrap();
        assert!((actual.0 - case["oracle"]["fwd"][0].as_f64().unwrap()).abs() < 0.001);
        assert!((actual.1 - case["oracle"]["fwd"][1].as_f64().unwrap()).abs() < 0.001);
        let inverse = Transformer::new(&target, &source).unwrap().transform(actual.0, actual.1).unwrap();
        assert!((inverse.0 - case["oracle"]["inv"][0].as_f64().unwrap()).abs() < 1e-8);
        assert!((inverse.1 - case["oracle"]["inv"][1].as_f64().unwrap()).abs() < 1e-8);
        let reopened = SpatialRef::parse(&target.to_wkt().unwrap()).unwrap();
        let again = Transformer::new(&source, &reopened).unwrap().transform(x, y).unwrap();
        assert!((again.0 - actual.0).abs() < 0.001 && (again.1 - actual.1).abs() < 0.001);
    }
}

fn angular_error(actual: (f64, f64), expected: &serde_json::Value) -> f64 {
    let lon = expected[0].as_f64().unwrap();
    let lat = expected[1].as_f64().unwrap();
    let dlon = (actual.0 - lon + 180.0).rem_euclid(360.0) - 180.0;
    (dlon * lat.to_radians().cos()).abs().max((actual.1 - lat).abs())
}

#[test]
fn all_aspects_figures_units_and_domains_match_frozen_proj() {
    let data: serde_json::Value = serde_json::from_str(include_str!("data/perspective_reference.json")).unwrap();
    assert_eq!(data["environment"]["proj"], "9.8.1");
    for case in data["cases"].as_array().unwrap() {
        let source = SpatialRef::parse(case["source"].as_str().unwrap()).unwrap();
        let target = SpatialRef::parse(case["target"].as_str().unwrap()).unwrap();
        let reopened = SpatialRef::parse(case["wkt2"].as_str().unwrap()).unwrap();
        let wkt_forward = Transformer::new(&source, &reopened).unwrap();
        let forward = Transformer::new(&source, &target).unwrap();
        let inverse = Transformer::new(&target, &source).unwrap();
        let factor = case["metres_per_unit"].as_f64().unwrap();
        for sample in case["samples"].as_array().unwrap() {
            let point = (sample[0].as_f64().unwrap(), sample[1].as_f64().unwrap());
            let actual = forward.transform(point.0, point.1);
            if sample[2].is_null() {
                assert!(actual.is_err(), "{}: {point:?} should be invisible", case["name"]);
                continue;
            }
            let actual = actual.unwrap_or_else(|e| panic!("{} {point:?}: {e}", case["name"]));
            let expected = (sample[2][0].as_f64().unwrap(), sample[2][1].as_f64().unwrap());
            let from_wkt = wkt_forward.transform(point.0, point.1).unwrap();
            assert!(
                (from_wkt.0 - expected.0).abs().max((from_wkt.1 - expected.1).abs()) * factor <= 0.001,
                "{} WKT2 {point:?}",
                case["name"]
            );
            let error = (actual.0 - expected.0).abs().max((actual.1 - expected.1).abs()) * factor;
            assert!(error <= 0.001, "{} {point:?}: forward error {error} m", case["name"]);
            let actual = inverse.transform(expected.0, expected.1);
            if sample[3].is_null() {
                assert!(actual.is_err(), "{}: inverse must refuse {expected:?}", case["name"]);
            } else {
                let actual = actual.unwrap_or_else(|e| panic!("{} inverse {point:?}: {e}", case["name"]));
                let error = angular_error(actual, &sample[3]);
                assert!(error <= 1e-8, "{} {point:?}: inverse error {error} degrees", case["name"]);
            }
        }
        for sample in case["inverse_samples"].as_array().unwrap() {
            let point = (sample[0][0].as_f64().unwrap(), sample[0][1].as_f64().unwrap());
            let actual = inverse.transform(point.0, point.1);
            if sample[1].is_null() {
                assert!(actual.is_err(), "{}: inverse domain {point:?}", case["name"]);
            } else {
                let actual = actual.unwrap_or_else(|e| panic!("{} inverse domain {point:?}: {e}", case["name"]));
                assert!(angular_error(actual, &sample[1]) <= 1e-8, "{}: inverse {point:?}", case["name"]);
            }
        }
    }
}

#[test]
fn perspective_rejects_invalid_parameters() {
    for definition in [
        "+proj=nsper",
        "+proj=tpers +h=0",
        "+proj=nsper +h=-1",
        "+proj=tpers +h=NaN",
        "+proj=nsper +h=1e30",
        "+proj=tpers +h=3000000 +tilt=NaN",
        "+proj=tpers +h=3000000 +azi=bad",
        "+proj=nsper +h=3000000 +lat_0=bad",
        "+proj=nsper +h=3000000 +lon_0=NaN",
        "+proj=nsper +h=3000000 +lat_0=91",
    ] {
        assert!(SpatialRef::parse(definition).is_err(), "{definition}");
    }
}

#[test]
fn wkt_vertical_perspective_does_not_ignore_topocentric_height() {
    let data: serde_json::Value = serde_json::from_str(include_str!("data/perspective_reference.json")).unwrap();
    let wkt = data["cases"][0]["wkt2"].as_str().unwrap();
    let changed = wkt.replace(
        "PARAMETER[\"Ellipsoidal height of topocentric origin\",0,",
        "PARAMETER[\"Ellipsoidal height of topocentric origin\",100,",
    );
    assert_ne!(wkt, changed);
    let target = SpatialRef::parse(&changed).unwrap();
    let source = SpatialRef::parse(data["cases"][0]["source"].as_str().unwrap()).unwrap();
    assert!(Transformer::new(&source, &target).is_err());
}
