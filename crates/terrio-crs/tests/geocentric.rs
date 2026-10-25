//! Geographic/Cartesian endpoints, pinned to rust-proj-capability's PROJ 9.9.0 oracle.
use terrio_crs::{SpatialRef, TransformOptions, Transformer};

fn transform(src: &str, dst: &str, p: (f64, f64, f64)) -> (f64, f64, f64) {
    Transformer::new(&SpatialRef::parse(src).unwrap(), &SpatialRef::parse(dst).unwrap()).unwrap().transform_3d(p.0, p.1, p.2).unwrap()
}
fn near(a: (f64, f64, f64), b: (f64, f64, f64), tolerance: f64) {
    assert!((a.0 - b.0).abs() <= tolerance && (a.1 - b.1).abs() <= tolerance && (a.2 - b.2).abs() <= tolerance, "{a:?} != {b:?}");
}

#[test]
fn pinned_geographic_to_geocentric() {
    // rust-proj-capability ddd9fac70ec370acd9651ebb8b46035207347ad5,
    // suites/crs/crs.json pair-4979-4978; frozen PROJ 9.9.0, pyproj 3.8.0, no grids.
    let oracle: serde_json::Value = serde_json::from_str(include_str!("data/geocentric_benchmark.json")).unwrap();
    assert_eq!(oracle["environment"]["proj_version"], "9.9.0");
    let expected = &oracle["fixture"]["oracle"]["fwd"];
    let actual = transform("EPSG:4979", "EPSG:4978", (147.0, -42.0, 100.0));
    near(actual, (expected[0].as_f64().unwrap(), expected[1].as_f64().unwrap(), expected[2].as_f64().unwrap()), 0.001);
    near(transform("EPSG:4978", "EPSG:4979", actual), (147.0, -42.0, 100.0), 1e-7);
    let cart = SpatialRef::from_epsg(4978).unwrap();
    assert!(cart.is_geocentric());
    assert!(!cart.is_geographic());
    assert_eq!(cart.geographic_code(), Some(4326));
}

#[test]
fn axes_units_poles_origin_and_invalid_domain() {
    let a = 6378137.0;
    near(transform("EPSG:4979", "EPSG:4978", (0.0, 0.0, 10.0)), (a + 10.0, 0.0, 0.0), 1e-8);
    near(transform("EPSG:4979", "EPSG:4978", (90.0, 0.0, 0.0)), (0.0, a, 0.0), 1e-8);
    for lat in [-90.0, 90.0] {
        let xyz = transform("EPSG:4979", "EPSG:4978", (0.0, lat, 50.0));
        near(transform("EPSG:4978", "EPSG:4979", xyz), (0.0, lat, 50.0), 1e-7);
    }
    let origin = transform("EPSG:4978", "EPSG:4979", (0.0, 0.0, 0.0));
    near(origin, (0.0, 90.0, -6356752.314245179), 1e-7);
    near(transform("EPSG:4979", "+proj=geocent +datum=WGS84 +units=km", (0.0, 0.0, 0.0)), (a / 1000.0, 0.0, 0.0), 1e-9);
    for (src, dst) in [("EPSG:4979", "EPSG:4978"), ("EPSG:4978", "EPSG:4979"), ("EPSG:4978", "EPSG:4978")] {
        let t = Transformer::new(&SpatialRef::parse(src).unwrap(), &SpatialRef::parse(dst).unwrap()).unwrap();
        assert!(t.transform_3d(f64::NAN, 0.0, 0.0).is_err());
        assert!(t.transform_3d(0.0, 0.0, f64::INFINITY).is_err());
    }
    let t = Transformer::new(&SpatialRef::from_epsg(4979).unwrap(), &SpatialRef::from_epsg(4978).unwrap()).unwrap();
    assert!(t.transform_3d(0.0, 90.1, 0.0).is_err());
}

#[test]
fn proj_and_wkt_definitions_and_projected_composition() {
    let geocent = SpatialRef::from_epsg(4978).unwrap();
    let wkt = geocent.to_wkt().unwrap();
    let reference = transform("EPSG:4979", "EPSG:4978", (12.0, 45.0, 123.0));
    near(transform("EPSG:4979", &wkt, (12.0, 45.0, 123.0)), reference, 0.001);
    let proj = "+proj=geocent +datum=WGS84 +units=m";
    let generated = SpatialRef::parse(proj).unwrap().to_wkt().unwrap();
    assert!(generated.starts_with("GEOCCS["));
    near(transform("EPSG:4979", &generated, (12.0, 45.0, 123.0)), reference, 0.001);
    let projected = transform("EPSG:4978", "EPSG:32633", reference);
    let direct = transform("EPSG:4979", "EPSG:32633", (12.0, 45.0, 123.0));
    near(projected, direct, 0.001);
    near(transform("EPSG:32633", "EPSG:4978", projected), reference, 0.001);
    let sphere = "+proj=geocent +R=1000 +units=m";
    let geographic = "+proj=longlat +R=1000";
    near(transform(geographic, sphere, (0.0, 90.0, 10.0)), (0.0, 0.0, 1010.0), 1e-9);
}

#[test]
fn frozen_gdal_composition_and_definition_oracles() {
    let oracle: serde_json::Value = serde_json::from_str(include_str!("data/geocentric_reference.json")).unwrap();
    for case in oracle["cases"].as_array().unwrap() {
        let src = SpatialRef::parse(case["src"].as_str().unwrap()).unwrap();
        let dst = SpatialRef::parse(case["dst"].as_str().unwrap()).unwrap();
        let options = TransformOptions { source_coordinate_epoch: case["source_epoch"].as_f64(), ..Default::default() };
        let t = Transformer::with_options(&src, &dst, &options).unwrap();
        for (point, expected) in case["points"].as_array().unwrap().iter().zip(case["expected"].as_array().unwrap()) {
            let (x, y, z) = t.transform_3d(point[0].as_f64().unwrap(), point[1].as_f64().unwrap(), point[2].as_f64().unwrap()).unwrap();
            for (i, actual) in [x, y, z].iter().enumerate() {
                let limit = case["tolerance"][i].as_f64().unwrap();
                assert!(
                    (actual - expected[i].as_f64().unwrap()).abs() <= limit,
                    "{} from {point}: component {i} = {actual}, expected {}, tolerance {limit}",
                    case["name"],
                    expected[i]
                );
            }
        }
    }
}

#[test]
fn malformed_geocentric_definitions_fail_explicitly() {
    for definition in ["+proj=geocent +units=nonsense", "+proj=geocent +to_meter=0", "+proj=geocent +a=-1 +b=1"] {
        assert!(SpatialRef::parse(definition).is_err(), "{definition}");
    }
    let reference: serde_json::Value = serde_json::from_str(include_str!("data/geocentric_reference.json")).unwrap();
    let wkt =
        reference["cases"].as_array().unwrap().iter().find(|c| c["name"] == "WKT2_2019 without IDs").unwrap()["dst"].as_str().unwrap();
    for modified in [wkt.replace("CS[Cartesian,3]", "CS[Cartesian,2]"), wkt.replace("CS[Cartesian,3]", "CS[spherical,3]")] {
        let crs = SpatialRef::from_wkt(&modified);
        assert!(Transformer::new(&crs, &SpatialRef::from_epsg(4979).unwrap()).is_err());
    }
}
