//! Existing Terrio callers and standalone callers share one engine without changing errors,
//! public CRS fields, format conversion behavior, registered grids or warning sinks.

use terrio::crs::{SpatialRef, TransformOptions, Transformer};

#[test]
fn public_types_errors_and_format_adapters_remain_compatible() {
    let error: terrio::Error = SpatialRef::parse("not a CRS").unwrap_err();
    assert!(matches!(error, terrio::Error::Crs(_)));
    let error: terrio::Error = TransformOptions::from_strict_options(&[("UNKNOWN", "YES")]).unwrap_err();
    assert!(matches!(error, terrio::Error::InvalidArgument(_)));

    let original = SpatialRef { epsg: Some(32613), wkt: None, proj4: None, esri: None };
    let core: terrio_crs::SpatialRef = original.clone().into();
    let roundtrip: SpatialRef = core.clone().into();
    assert_eq!(original, roundtrip);
    assert_eq!(serde_json::to_value(&original).unwrap(), serde_json::to_value(&core).unwrap());
    assert_eq!(original.to_string(), core.to_string());
    assert_eq!(original.to_wkt(), core.to_wkt());
    let info = original.to_crs_info();
    assert_eq!(info.epsg, Some(32613));
    assert_eq!(info.wkt, original.to_wkt());
    let mut layer = wbvector::Layer::new("test");
    original.apply_to_layer(&mut layer);
    assert!(original.same_as(&SpatialRef::from_layer(&layer)));

    let source = SpatialRef::from_epsg(4326).unwrap();
    let wrapped = Transformer::new(&source, &original).unwrap();
    let direct = terrio_crs::Transformer::new(&source.into(), &core).unwrap();
    assert_eq!(wrapped.transform(-105.0, 40.0).unwrap(), direct.transform(-105.0, 40.0).unwrap());
    let error: terrio::Error = wrapped.transform(f64::NAN, 40.0).unwrap_err();
    assert!(matches!(error, terrio::Error::Crs(_)));
}

fn grid(value: f32) -> Vec<u8> {
    // A 2x2 GTX geoid covering (0,0) to (1,1), big-endian as the format specifies.
    let mut bytes = Vec::new();
    for v in [0.0_f64, 0.0, 1.0, 1.0] {
        bytes.extend(v.to_be_bytes());
    }
    for n in [2_i32, 2] {
        bytes.extend(n.to_be_bytes());
    }
    for _ in 0..4 {
        bytes.extend(value.to_be_bytes());
    }
    bytes
}

#[test]
fn registered_grids_are_shared_in_both_directions() {
    terrio_crs::register_grid("compat-core.gtx", grid(12.0));
    terrio::crs::register_grid("compat-terrio.gtx", grid(24.0));
    for (name, shift) in [("compat-core.gtx", 12.0), ("compat-terrio.gtx", 24.0)] {
        let source = format!("+proj=longlat +ellps=WGS84 +geoidgrids={name}");
        let target = "EPSG:4979";
        let wrapped = Transformer::new(&SpatialRef::parse(&source).unwrap(), &SpatialRef::parse(target).unwrap()).unwrap();
        let direct =
            terrio_crs::Transformer::new(&terrio_crs::SpatialRef::parse(&source).unwrap(), &terrio_crs::SpatialRef::parse(target).unwrap())
                .unwrap();
        let result = wrapped.transform_3d(0.5, 0.5, 100.0).unwrap();
        assert_eq!(result, direct.transform_3d(0.5, 0.5, 100.0).unwrap());
        assert!((result.2 - 100.0 - shift).abs() < 1e-8, "{name}: {result:?}");
    }
}

#[test]
fn direct_and_terrio_warning_sinks_remain_independent() {
    let _ = terrio::warnings::take();
    let _ = terrio_crs::take_warnings();
    let source = "EPSG:4610";
    let target = "EPSG:4326";
    {
        let transform = Transformer::new(&SpatialRef::parse(source).unwrap(), &SpatialRef::parse(target).unwrap()).unwrap();
        transform.transform(110.0, 35.0).unwrap();
        transform.flush_warnings();
    }
    assert!(terrio::warnings::take().iter().any(|w| w.contains("ballpark")));
    assert!(terrio_crs::take_warnings().is_empty());
    {
        let transform =
            terrio_crs::Transformer::new(&terrio_crs::SpatialRef::parse(source).unwrap(), &terrio_crs::SpatialRef::parse(target).unwrap())
                .unwrap();
        transform.transform(110.0, 35.0).unwrap();
    }
    assert!(terrio_crs::take_warnings().iter().any(|w| w.contains("ballpark")));
    assert!(terrio::warnings::take().is_empty());
}
