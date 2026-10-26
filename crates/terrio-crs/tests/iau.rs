use terrio_crs::{SpatialRef, TransformOptions, Transformer};

#[test]
fn pinned_mars_fixture() {
    let data: serde_json::Value = serde_json::from_str(include_str!("data/iau_benchmark.json")).unwrap();
    assert_eq!(data["environment"]["proj_version"], "9.9.0");
    let case = &data["fixture"];
    let source = SpatialRef::parse(case["src"].as_str().unwrap()).unwrap();
    let target = SpatialRef::parse(case["dst"].as_str().unwrap()).unwrap();
    assert_eq!(source.authority(), Some(("IAU_2015", 49900)));
    assert_eq!(target.authority_code().as_deref(), Some("IAU_2015:49910"));
    let point = (case["point"][0].as_f64().unwrap(), case["point"][1].as_f64().unwrap());
    let result = Transformer::new(&source, &target).unwrap().transform(point.0, point.1).unwrap();
    assert!((result.0 - case["oracle"]["fwd"][0].as_f64().unwrap()).abs() < 0.001);
    assert!((result.1 - case["oracle"]["fwd"][1].as_f64().unwrap()).abs() < 0.001);
    let inverse = Transformer::new(&target, &source).unwrap().transform(result.0, result.1).unwrap();
    assert!((inverse.0 - point.0).abs() < 1e-8);
    assert!((inverse.1 - point.1).abs() < 1e-8);
}

#[test]
fn authority_versions_and_unknown_codes() {
    for text in [
        "IAU_2015:49900",
        "IAU:49900",
        "IAU:2015:49900",
        "urn:ogc:def:crs:IAU:2015:49900",
        "urn:ogc:def:crs:IAU_2015::49900",
        "https://www.opengis.net/def/crs/IAU/2015/49900",
    ] {
        assert_eq!(SpatialRef::parse(text).unwrap().authority(), Some(("IAU_2015", 49900)), "{text}");
    }
    for text in
        ["IAU_2000:49900", "IAU:2020:49900", "IAU:0:49900", "IAU::49900", "IAU_2015:999999999", "IAU_2015:-49900", "IAU_2015:49900/extra"]
    {
        assert!(SpatialRef::parse(text).is_err(), "{text}");
    }
}

#[test]
fn planetocentric_latitude_and_west_positive_axes() {
    let source = SpatialRef::parse("IAU_2015:49902").unwrap();
    let target = SpatialRef::parse("IAU_2015:49912").unwrap();
    let result = Transformer::new(&source, &target).unwrap().transform(10.0, 45.0).unwrap();
    // Independent GDAL 3.12.2 / PROJ 9.8.1, traditional GIS order.
    assert!((result.0 - 592746.9752330622).abs() < 0.001);
    assert!((result.1 - 2664514.735491729).abs() < 0.001);
    let source = SpatialRef::parse("IAU_2015:49901").unwrap();
    let target = SpatialRef::parse("IAU_2015:49911").unwrap();
    let result = Transformer::new(&source, &target).unwrap().transform(10.0, 45.0).unwrap();
    assert!((result.0 - 2667361.3885487798).abs() < 0.001);
    assert!((result.1 - 585893.7980220814).abs() < 0.001);
}

#[test]
fn different_celestial_bodies_have_no_earth_datum_route() {
    let source = SpatialRef::parse("IAU_2015:49900").unwrap();
    for target in ["EPSG:4326", "EPSG:4267", "IAU_2015:30100", "+proj=longlat +datum=NAD83"] {
        let target = SpatialRef::parse(target).unwrap();
        assert!(Transformer::new(&source, &target).is_err());
        assert!(Transformer::new(&target, &source).is_err());
    }
}

#[test]
fn registry_wkt_reopening_preserves_planetary_coordinates() {
    for (source, target, point) in [(49902, 49912, (10.0, 45.0)), (49901, 49911, (10.0, 45.0)), (49900, 49910, (137.4, -4.6))] {
        let a = SpatialRef::from_iau(source).unwrap();
        let b = SpatialRef::from_iau(target).unwrap();
        let expected = Transformer::new(&a, &b).unwrap().transform(point.0, point.1).unwrap();
        let a = SpatialRef::parse(&a.to_wkt().unwrap()).unwrap();
        let b = SpatialRef::parse(&b.to_wkt().unwrap()).unwrap();
        let result = Transformer::new(&a, &b).unwrap().transform(point.0, point.1).unwrap();
        assert!((result.0 - expected.0).abs() < 0.001, "{source} -> {target}: {result:?}, {expected:?}");
        assert!((result.1 - expected.1).abs() < 0.001, "{source} -> {target}: {result:?}, {expected:?}");
    }
}

#[test]
fn every_registry_definition_matches_frozen_gdal_coordinates() {
    use std::io::Read;
    let mut bytes = &include_bytes!("../data/iau.tsv.zst")[..];
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(&mut bytes).unwrap();
    let mut text = String::new();
    decoder.read_to_string(&mut text).unwrap();
    let rows: std::collections::HashMap<u32, Vec<&str>> = text
        .lines()
        .map(|line| {
            let row: Vec<_> = line.splitn(9, '\t').collect();
            (row[0].parse().unwrap(), row)
        })
        .collect();
    assert_eq!(rows.len(), 2201);
    for code in rows.keys() {
        let original = SpatialRef::from_iau(*code).unwrap_or_else(|error| panic!("IAU:{code}: {error}"));
        assert_eq!(original.authority(), Some(("IAU_2015", *code)));
        let reopened = SpatialRef::parse(&original.to_wkt().unwrap()).unwrap();
        Transformer::with_options(&original, &reopened, &TransformOptions { allow_ballpark: Some(false), ..Default::default() })
            .unwrap_or_else(|error| panic!("IAU:{code} WKT reopening: {error}"));
    }
    let reference: serde_json::Value = serde_json::from_str(include_str!("data/iau_reference.json")).unwrap();
    let mut failures = Vec::new();
    for case in reference["cases"].as_array().unwrap() {
        let code = case["code"].as_u64().unwrap() as u32;
        let base = case["base"].as_u64().unwrap() as u32;
        let source = SpatialRef::from_iau(base).unwrap();
        let target = SpatialRef::from_iau(code).unwrap();
        let strict = TransformOptions { allow_ballpark: Some(false), ..Default::default() };
        let forward = Transformer::with_options(&source, &target, &strict).unwrap();
        let inverse = Transformer::with_options(&target, &source, &strict).unwrap();
        for (i, point) in case["points"].as_array().unwrap().iter().enumerate() {
            let p = (point[0].as_f64().unwrap(), point[1].as_f64().unwrap());
            let oracle = &case["forward"][i];
            let expected = (oracle[0].as_f64().unwrap(), oracle[1].as_f64().unwrap());
            match forward.transform(p.0, p.1) {
                Ok(actual) => {
                    let error = (actual.0 - expected.0).abs().max((actual.1 - expected.1).abs());
                    if error > 0.001 {
                        failures.push(format!("FWD {code} point {i}: {error} metres; {actual:?} vs {expected:?}"));
                    }
                }
                Err(error) => failures.push(format!("FWD {code} point {i}: {error}")),
            }
            let back = inverse.transform(expected.0, expected.1);
            let oracle = &case["inverse"][i];
            let compare = |actual: (f64, f64), expected: (f64, f64)| {
                let mut maximum: f64 = 0.0;
                for (j, (a, b)) in [(actual.0, expected.0), (actual.1, expected.1)].into_iter().enumerate() {
                    let delta =
                        if matches!(rows[&base][4].as_bytes()[j], b'e' | b'w') { (a - b + 180.0).rem_euclid(360.0) - 180.0 } else { a - b };
                    maximum = maximum.max(delta.abs());
                }
                maximum
            };
            if oracle.is_null() {
                // If GDAL refuses an inverse, native refusal or accurate recovery
                // of the original point is accepted and reported separately.
                if let Ok(actual) = back {
                    let error = compare(actual, p);
                    if error > 1e-8 {
                        failures.push(format!("INV-REFUSED {code} point {i}: {error} degrees; {actual:?} vs original {p:?}"));
                    }
                }
            } else {
                let expected = (oracle[0].as_f64().unwrap(), oracle[1].as_f64().unwrap());
                match back {
                    Ok(actual) => {
                        let error = compare(actual, expected);
                        if error > 1e-8 {
                            failures.push(format!("INV {code} point {i}: {error} degrees; {actual:?} vs {expected:?}"));
                        }
                    }
                    Err(error) => failures.push(format!("INV {code} point {i}: {error}")),
                }
            }
        }
    }
    for failure in &failures {
        eprintln!("{failure}");
    }
    assert!(failures.is_empty(), "{} forward/inverse coordinate comparisons differ", failures.len());
}

#[test]
fn planetary_units_offsets_and_conformal_radius_match_gdal() {
    let reference: serde_json::Value = serde_json::from_str(include_str!("data/iau_reference.json")).unwrap();
    for case in reference["extras"].as_array().unwrap() {
        let source = SpatialRef::parse(case["source"].as_str().unwrap()).unwrap();
        let target = SpatialRef::parse(case["target"].as_str().unwrap()).unwrap();
        let forward = Transformer::new(&source, &target).unwrap();
        let inverse = Transformer::new(&target, &source).unwrap();
        let factor = case["metres_per_unit"].as_f64().unwrap();
        for (i, p) in case["points"].as_array().unwrap().iter().enumerate() {
            let result = forward.transform(p[0].as_f64().unwrap(), p[1].as_f64().unwrap()).unwrap();
            let expected = &case["forward"][i];
            assert!((result.0 - expected[0].as_f64().unwrap()).abs() * factor < 0.001, "{}", case["name"]);
            assert!((result.1 - expected[1].as_f64().unwrap()).abs() * factor < 0.001, "{}", case["name"]);
            let result = inverse.transform(expected[0].as_f64().unwrap(), expected[1].as_f64().unwrap()).unwrap();
            let expected = &case["inverse"][i];
            assert!((result.0 - expected[0].as_f64().unwrap()).abs() < 1e-8, "{}", case["name"]);
            assert!((result.1 - expected[1].as_f64().unwrap()).abs() < 1e-8, "{}", case["name"]);
        }
    }
    let source = SpatialRef::from_iau(49900).unwrap();
    let earth_shift = SpatialRef::parse("+proj=longlat +a=3396190 +b=3376200 +towgs84=1,2,3").unwrap();
    assert!(Transformer::new(&source, &earth_shift).is_err());
}

#[test]
fn strict_datum_selection_preserves_iau_identity() {
    let strict = TransformOptions { allow_ballpark: Some(false), ..Default::default() };
    let sphere = SpatialRef::parse("IAU_2015:49900").unwrap();
    for target in ["IAU_2015:49901", "+proj=eqc +R=3396190 +units=km"] {
        let target = SpatialRef::parse(target).unwrap();
        assert!(Transformer::new(&sphere, &target).is_ok());
        assert!(Transformer::with_options(&sphere, &target, &strict).is_err());
    }
    let edited = SpatialRef::parse(&sphere.wkt.as_deref().unwrap().replacen("DATUM[\"", "DATUM[\"Edited ", 1)).unwrap();
    assert!(Transformer::with_options(&sphere, &edited, &strict).is_err());
    for (source, target) in [("IAU_2015:49901", "IAU_2015:49902"), ("IAU_2015:49900", "IAU_2015:49910")] {
        let source = SpatialRef::parse(source).unwrap();
        let target = SpatialRef::parse(target).unwrap();
        assert!(Transformer::with_options(&source, &target, &strict).is_ok());
        let reopened = SpatialRef::parse(source.wkt.as_deref().unwrap()).unwrap();
        assert!(Transformer::with_options(&reopened, &target, &strict).is_ok());
    }
}

#[test]
fn edited_iau_parameters_cannot_short_circuit_transformations() {
    let source = SpatialRef::from_iau(49910).unwrap();
    let wkt = source.to_wkt().unwrap();
    let edited = wkt.replacen("PARAMETER[\"False easting\",0,", "PARAMETER[\"False easting\",1234,", 1);
    assert_ne!(wkt, edited);
    let target = SpatialRef::parse(&edited).unwrap();
    assert_eq!(target.authority(), None);
    assert!(!source.same_as(&target));
    let result = Transformer::new(&source, &target).unwrap().transform(0.0, 0.0).unwrap();
    assert!((result.0 - 1234.0).abs() < 1e-8);
    assert!(result.1.abs() < 1e-8);
    let shifted = wkt.replacen("PRIMEM[\"Reference Meridian\",0,", "PRIMEM[\"Reference Meridian\",10,", 1);
    assert_ne!(shifted, wkt);
    let target = SpatialRef::parse(&shifted).unwrap();
    assert_eq!(target.authority(), None);
    assert!(!source.same_as(&target));
    let result = Transformer::new(&source, &target).unwrap().transform(0.0, 0.0).unwrap();
    assert!((result.0 + 592746.9752330622).abs() < 0.001);
    assert!(result.1.abs() < 1e-8);
    let mut overridden = source.clone();
    overridden.proj4 = Some(source.proj4.as_deref().unwrap().replace("+x_0=0", "+x_0=1234"));
    assert_eq!(overridden.authority(), None);
    assert!(!source.same_as(&overridden));
    let result = Transformer::new(&source, &overridden).unwrap().transform(0.0, 0.0).unwrap();
    assert!((result.0 - 1234.0).abs() < 1e-8);
}

#[test]
fn custom_ellipsoid_body_compatibility_applies_without_iau_ids() {
    for (source, target, allowed) in [
        ("+proj=longlat +R=1", "+proj=longlat +R=2", false),
        ("+proj=longlat +R=1", "EPSG:4326", false),
        ("+proj=longlat +R=1737400", "EPSG:4326", false),
        ("+proj=longlat +R=6371000", "IAU_2015:39900", true),
        ("+proj=longlat +R=1", "+proj=eqc +R=1", true),
        ("+proj=longlat +R=1", "+proj=longlat +R=1.006", true),
        ("+proj=longlat +R=1", "+proj=longlat +R=1.008", false),
    ] {
        let source = SpatialRef::parse(source).unwrap();
        let target = SpatialRef::parse(target).unwrap();
        let result = Transformer::new(&source, &target);
        if allowed {
            assert!(result.unwrap().transform(1.0, 2.0).is_ok());
        } else {
            assert!(result.err().unwrap().to_string().contains("celestial body"));
        }
    }
    // An explicit non-Earth body wins even if its edited radius is Earth-like.
    let source = SpatialRef::parse("+proj=longlat +R=6371000").unwrap();
    let mars = SpatialRef::from_iau(49900).unwrap().to_wkt().unwrap();
    let earth_sized_mars = mars.replace("3396190", "6371000");
    assert_ne!(mars, earth_sized_mars);
    let target = SpatialRef::parse(&earth_sized_mars).unwrap();
    assert!(Transformer::new(&source, &target).err().unwrap().to_string().contains("celestial body"));
    for shift in ["+towgs84=1,2,3", "+datum=WGS84"] {
        let source = SpatialRef::parse(&format!("+proj=longlat +a=1 +b=1 {shift}")).unwrap();
        let target = SpatialRef::parse("+proj=longlat +R=1").unwrap();
        assert!(Transformer::new(&source, &target).err().unwrap().to_string().contains("Earth datum operations"));
    }
}

#[test]
fn conformal_radius_geographic_and_geocentric_endpoints_match_proj() {
    // GDAL 3.12.2 / PROJ 9.8.1: metadata retains WGS84 while the Cartesian
    // conversion uses its equatorial conformal radius (the semi-minor axis).
    let source = SpatialRef::from_epsg(4979).unwrap();
    let geographic = SpatialRef::parse("+proj=longlat +ellps=WGS84 +R_C").unwrap();
    let point = Transformer::new(&source, &geographic).unwrap().transform_3d(17.0, 45.0, 100.0).unwrap();
    assert!((point.0 - 17.0).abs() < 1e-8 && (point.1 - 45.0).abs() < 1e-8 && (point.2 - 100.0).abs() < 1e-5);
    assert_eq!(terrio_crs::semi_major_axis(&geographic.to_wkt().unwrap()), Some(6378137.0));
    let target = SpatialRef::parse("+proj=geocent +ellps=WGS84 +R_C").unwrap();
    assert_eq!(terrio_crs::semi_major_axis(&target.to_wkt().unwrap()), Some(6378137.0));
    let point = Transformer::new(&source, &target).unwrap().transform_3d(17.0, 45.0, 100.0).unwrap();
    let expected = (4298564.419695135, 1314203.029327345, 4494973.378404164);
    assert!((point.0 - expected.0).abs() < 0.001 && (point.1 - expected.1).abs() < 0.001 && (point.2 - expected.2).abs() < 0.001);
    let reopened = SpatialRef::parse(&target.to_wkt().unwrap()).unwrap();
    let again = Transformer::new(&source, &reopened).unwrap().transform_3d(17.0, 45.0, 100.0).unwrap();
    assert!((again.0 - point.0).abs() < 0.001 && (again.1 - point.1).abs() < 0.001 && (again.2 - point.2).abs() < 0.001);
    let back = Transformer::new(&target, &source).unwrap().transform_3d(point.0, point.1, point.2).unwrap();
    assert!((back.0 - 17.0).abs() < 1e-8 && (back.1 - 45.0).abs() < 1e-8 && (back.2 - 100.0).abs() < 1e-5);
}
