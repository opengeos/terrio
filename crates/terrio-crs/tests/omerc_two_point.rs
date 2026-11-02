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
        let written = target.to_wkt().unwrap();
        assert!(target.to_projjson().is_err(), "two-point PROJJSON must not become an azimuth method");
        let encodings: Vec<(Transformer, Transformer)> =
            [case["wkt2"].as_str().unwrap(), case["wkt1"].as_str().unwrap(), case["wkt1_esri"].as_str().unwrap(), &written]
                .iter()
                .map(|text| {
                    let encoded = SpatialRef::parse(text).unwrap();
                    (Transformer::new(&source, &encoded).unwrap(), Transformer::new(&encoded, &source).unwrap())
                })
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
            for (encoding, (t, reverse)) in ["WKT2", "WKT1", "ESRI WKT1", "written WKT1"].iter().zip(&encodings) {
                let got = t.transform(point.0, point.1).unwrap();
                let error = (got.0 - expected.0).abs().max((got.1 - expected.1).abs()) * factor;
                assert!(error <= 0.001, "{name} {encoding} {point:?}: forward error {error} m");
                let back = reverse.transform(expected.0, expected.1);
                if sample[3].is_null() {
                    assert!(back.is_err(), "{name} {encoding}: inverse must refuse {expected:?}");
                } else {
                    let back = back.unwrap_or_else(|e| panic!("{name} {encoding} inverse {point:?}: {e}"));
                    assert!(angular_error(back, &sample[3]) <= 1e-8, "{name} {encoding}: inverse {point:?}");
                }
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
    // Terrio also refuses nonnumeric and nonfinite parameter values.
    for definition in [
        "+proj=omerc +lat_1=bad +lon_1=0 +lat_2=40 +lon_2=10",
        "+proj=omerc +lat_1=40 +lon_1=0 +lat_2=45 +lon_2=NaN",
        "+proj=omerc +alpha=bad +lat_1=40 +lon_1=0 +lat_2=45 +lon_2=10",
    ] {
        assert!(SpatialRef::parse(definition).is_err(), "{definition}");
    }
}

/// Singularities and their finite neighbours, checked with GDAL 3.13.3 / PROJ 9.9.0.
#[test]
fn line_poles_are_refused_and_both_sides_remain_finite() {
    let source = SpatialRef::parse("+proj=longlat +R=6371000").unwrap();
    let target = SpatialRef::parse("+proj=omerc +lat_1=10 +lon_1=20 +lat_2=40 +lon_2=20 +R=6371000").unwrap();
    let forward = Transformer::new(&source, &target).unwrap();
    let inverse = Transformer::new(&target, &source).unwrap();
    for lon in [-70.0, 110.0] {
        assert!(forward.transform(lon, 0.0).is_err(), "line pole {lon}");
    }
    for (lon, x, y) in [
        (109.9, 44877062.691809826, 0.0),
        (110.1, 44877062.691809826, 20015086.79602057),
        (-69.9, -44877062.691809826, 0.0),
        (-70.1, -44877062.691809826, 20015086.79602057),
    ] {
        let actual = forward.transform(lon, 0.0).unwrap();
        assert!((actual.0 - x).abs() <= 0.001 && (actual.1 - y).abs() <= 0.001);
        let back = inverse.transform(actual.0, actual.1).unwrap();
        assert!((back.0 - lon).abs() <= 1e-8 && back.1.abs() <= 1e-8);
    }
    // Geographic poles are finite; they are different points from the line's singularities.
    for lat in [-90.0, 90.0] {
        let actual = forward.transform(20.0, lat).unwrap();
        assert!(actual.0.abs() <= 0.001);
        assert!((actual.1 - lat.signum() * 10007543.398010286).abs() <= 0.001);
    }
}

/// Malformed point values must not become omitted defaults in GDAL 3.13.3 WKT1 dialects.
#[test]
fn wkt_point_parameters_refuse_present_invalid_values() {
    let data: serde_json::Value = serde_json::from_str(include_str!("data/omerc_two_point_reference.json")).unwrap();
    let case = &data["cases"][0];
    let source = SpatialRef::parse(case["source"].as_str().unwrap()).unwrap();
    for (dialect, names) in [
        ("wkt1", ["latitude_of_point_1", "longitude_of_point_1", "latitude_of_point_2", "longitude_of_point_2"]),
        ("wkt1_esri", ["Latitude_Of_1st_Point", "Longitude_Of_1st_Point", "Latitude_Of_2nd_Point", "Longitude_Of_2nd_Point"]),
    ] {
        let wkt = case[dialect].as_str().unwrap();
        for name in names {
            let prefix = format!("PARAMETER[\"{name}\",");
            let start = wkt.find(&prefix).unwrap() + prefix.len();
            let end = start + wkt[start..].find(']').unwrap();
            for value in ["\"bad\"", "NaN", "1e999"] {
                let malformed = format!("{}{value}{}", &wkt[..start], &wkt[end..]);
                let operation = SpatialRef::parse(&malformed).and_then(|target| Transformer::new(&source, &target));
                assert!(operation.is_err(), "{dialect}: {name}={value} must be refused");
            }
        }
    }
}
