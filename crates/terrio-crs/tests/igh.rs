use terrio_crs::{SpatialRef, Transformer};

#[test]
fn pinned_interrupted_goode_homolosine_fixture() {
    let data: serde_json::Value = serde_json::from_str(include_str!("data/igh_benchmark.json")).unwrap();
    assert_eq!(data["environment"]["proj_version"], "9.9.0");
    for case in data["fixtures"].as_array().unwrap() {
        assert_eq!(case["id"], "seed-igh");
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
        assert!(written.contains("PROJECTION[\"Interrupted_Goode_Homolosine\"]"), "{written}");
        for wkt in [wkt1, written.as_str()] {
            let reopened = SpatialRef::parse(wkt).unwrap();
            let again = Transformer::new(&source, &reopened).unwrap().transform(x, y).unwrap();
            assert!((again.0 - actual.0).abs() < 0.001 && (again.1 - actual.1).abs() < 0.001, "{wkt}");
        }
    }
}

#[path = "common/angular.rs"]
mod angular;
use angular::angular_error;

#[test]
fn all_zones_figures_units_and_interruptions_match_frozen_proj() {
    let data: serde_json::Value = serde_json::from_str(include_str!("data/igh_reference.json")).unwrap();
    assert_eq!(data["environment"]["proj"], "9.8.1");
    for case in data["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let source = SpatialRef::parse(case["source"].as_str().unwrap()).unwrap();
        let factor = case["metres_per_unit"].as_f64().unwrap();
        for encoding in ["target", "wkt1", "wkt1_esri", "wkt2"] {
            let target = SpatialRef::parse(case[encoding].as_str().unwrap()).unwrap_or_else(|e| panic!("{name} {encoding}: {e}"));
            let forward = Transformer::new(&source, &target).unwrap();
            let inverse = Transformer::new(&target, &source).unwrap();
            for sample in case["samples"].as_array().unwrap() {
                let point = (sample[0].as_f64().unwrap(), sample[1].as_f64().unwrap());
                let expected = (sample[2][0].as_f64().unwrap(), sample[2][1].as_f64().unwrap());
                let actual = forward.transform(point.0, point.1).unwrap_or_else(|e| panic!("{name} {encoding} {point:?}: {e}"));
                let error = (actual.0 - expected.0).abs().max((actual.1 - expected.1).abs()) * factor;
                assert!(error <= 0.001, "{name} {encoding} {point:?}: forward error {error} m");
                let actual = inverse.transform(expected.0, expected.1);
                if sample[3].is_null() {
                    // Only the PROJ string is exact: a WKT's rounded unit (US survey feet) moves
                    // points on the interruptions' edges to either side of them.
                    if encoding == "target" {
                        assert!(actual.is_err(), "{name}: inverse must refuse {expected:?}, as PROJ does: {actual:?}");
                    }
                } else {
                    let actual = actual.unwrap_or_else(|e| panic!("{name} {encoding} inverse {point:?}: {e}"));
                    let error = angular_error(actual, &sample[3]);
                    assert!(error <= 1e-8, "{name} {encoding} {point:?}: inverse error {error} degrees");
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

/// Whether a CRS definition transforms from WGS 84 (definitions are read when used).
fn usable(definition: &str) -> bool {
    let geographic = SpatialRef::parse("+proj=longlat +datum=WGS84").unwrap();
    SpatialRef::parse(definition).and_then(|crs| Transformer::new(&geographic, &crs)?.transform(10.0, 50.0)).is_ok()
}

#[test]
fn other_goode_homolosines_are_refused_not_read_as_interrupted() {
    let geogcs = r#"GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#;
    let esri = |option: &str| {
        format!(
            r#"PROJCS["x",{geogcs},PROJECTION["Goode_Homolosine"],PARAMETER["False_Easting",0.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",0.0]{option},UNIT["Meter",1.0]]"#
        )
    };
    assert!(usable(&esri(r#",PARAMETER["Option",1.0]"#)));
    // PROJ's uninterrupted `goode` (no Option, or 0) and oceanic `igh_o` (Option 2).
    for option in ["", r#",PARAMETER["Option",0.0]"#, r#",PARAMETER["Option",2.0]"#] {
        assert!(!usable(&esri(option)), "{option}");
    }
    let data: serde_json::Value = serde_json::from_str(include_str!("data/igh_reference.json")).unwrap();
    let wkt2 = data["cases"][0]["wkt2"].as_str().unwrap();
    assert!(usable(wkt2));
    for method in ["Goode Homolosine", "Interrupted Goode Homolosine Ocean"] {
        let other = wkt2.replace("METHOD[\"Interrupted Goode Homolosine\"]", &format!("METHOD[\"{method}\"]"));
        assert_ne!(other, wkt2);
        assert!(!usable(&other), "{method}");
    }
}
