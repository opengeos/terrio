use terrio_crs::{SpatialRef, Transformer};

fn angular_error(actual: (f64, f64), expected: &serde_json::Value) -> f64 {
    let lon = expected[0].as_f64().unwrap();
    let lat = expected[1].as_f64().unwrap();
    let dlon = (actual.0 - lon + 180.0).rem_euclid(360.0) - 180.0;
    (dlon * lat.to_radians().cos()).abs().max((actual.1 - lat).abs())
}

#[test]
fn every_configuration_figure_and_unit_matches_frozen_proj() {
    let data: serde_json::Value = serde_json::from_str(include_str!("data/isea_reference.json")).unwrap();
    assert_eq!(data["environment"]["proj"], "9.8.1");
    for case in data["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let source = SpatialRef::parse(case["source"].as_str().unwrap()).unwrap();
        let factor = case["metres_per_unit"].as_f64().unwrap();
        // terrio's own WKT1, written from the PROJ string, is checked as the others.
        let written = SpatialRef::parse(case["target"].as_str().unwrap()).unwrap().to_wkt().unwrap();
        assert!(written.contains("PROJECTION[\"custom_proj4\"]"), "{written}");
        for encoding in ["target", "wkt1", "wkt1_esri", "wkt2", "written"] {
            let text = if encoding == "written" { written.as_str() } else { case[encoding].as_str().unwrap() };
            let target = SpatialRef::parse(text).unwrap_or_else(|e| panic!("{name} {encoding}: {e}"));
            let forward = Transformer::new(&source, &target).unwrap_or_else(|e| panic!("{name} {encoding}: {e}"));
            let inverse = Transformer::new(&target, &source).unwrap_or_else(|e| panic!("{name} {encoding}: {e}"));
            for sample in case["samples"].as_array().unwrap() {
                let point = (sample[0].as_f64().unwrap(), sample[1].as_f64().unwrap());
                let expected = (sample[2][0].as_f64().unwrap(), sample[2][1].as_f64().unwrap());
                let actual = forward.transform(point.0, point.1).unwrap_or_else(|e| panic!("{name} {encoding} {point:?}: {e}"));
                let error = (actual.0 - expected.0).abs().max((actual.1 - expected.1).abs()) * factor;
                assert!(error <= 0.001, "{name} {encoding} {point:?}: forward error {error} m");
                // PROJ's inverse, which is not the forward's on an ellipsoid and is refused
                // outside its default grid.
                let actual = inverse.transform(expected.0, expected.1);
                if sample[3].is_null() {
                    assert!(actual.is_err(), "{name} {encoding}: inverse must refuse {expected:?}, as PROJ does: {actual:?}");
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
                    assert!(actual.is_err(), "{name} {encoding}: inverse must refuse {point:?}, as PROJ does: {actual:?}");
                } else {
                    let actual = actual.unwrap_or_else(|e| panic!("{name} {encoding} inverse {point:?}: {e}"));
                    let error = angular_error(actual, &sample[1]);
                    assert!(error <= 1e-8, "{name} {encoding}: inverse {point:?} error {error} degrees");
                }
            }
        }
        // PROJ has no EPSG method for it, so PROJJSON export refuses it.
        assert!(SpatialRef::parse(case["target"].as_str().unwrap()).unwrap().to_projjson().is_err(), "{name}");
    }
}

#[test]
fn invalid_and_grid_address_options_are_refused() {
    let lonlat = SpatialRef::parse("+proj=longlat +R=1").unwrap();
    // PROJ 9.8.1 refuses the first eight, as gen_isea_reference.py checks. The grid addresses of
    // +mode=di, dd and hex are not coordinates, and terrio does not compute them.
    for bad in [
        "+orient=foo",
        "+orient",
        "+mode=foo",
        "+mode",
        "+resolution=4.0",
        "+aperture=-3",
        "+resolution=99999999999999999999x",
        "+azi=30x",
        "+mode=di",
        "+mode=dd",
        "+mode=hex",
    ] {
        let crs = format!("+proj=isea +R=1 {bad}");
        assert!(SpatialRef::parse(&crs).and_then(|t| Transformer::new(&lonlat, &t)).is_err(), "{crs}");
    }
    for good in ["+orient=isea", "+orient=pole", "+mode=plane", "+resolution=4 +aperture=3", "+resolution", "+azi"] {
        let crs = format!("+proj=isea +R=1 {good}");
        assert!(SpatialRef::parse(&crs).and_then(|t| Transformer::new(&lonlat, &t)).is_ok(), "{crs}");
    }
}

#[test]
fn wkt2_method_name_parameters_are_checked() {
    let wkt = |method: &str| {
        format!(
            "PROJCRS[\"unknown\",BASEGEOGCRS[\"unknown\",DATUM[\"unknown\",ELLIPSOID[\"unknown\",6371007.18091847,0,\
             LENGTHUNIT[\"metre\",1]]],PRIMEM[\"Greenwich\",0,ANGLEUNIT[\"degree\",0.0174532925199433]]],\
             CONVERSION[\"unknown\",METHOD[\"{method}\"],PARAMETER[\"x_0\",0,LENGTHUNIT[\"metre\",1]]],\
             CS[Cartesian,2],AXIS[\"(E)\",east,ORDER[1],LENGTHUNIT[\"metre\",1]],\
             AXIS[\"(N)\",north,ORDER[2],LENGTHUNIT[\"metre\",1]]]"
        )
    };
    let lonlat = SpatialRef::parse("+proj=longlat +R=6371007.18091847").unwrap();
    let transformer = |method: &str| SpatialRef::parse(&wkt(method)).and_then(|t| Transformer::new(&lonlat, &t));
    assert!(transformer("PROJ isea orient=pole").is_ok());
    // The figure, datum, units and grids come from the CRS, and the false origin is a PARAMETER.
    for method in [
        "PROJ isea proj=merc",
        "PROJ isea nadgrids=grid",
        "PROJ isea units=km",
        "PROJ isea R=1",
        "PROJ isea R_C=1",
        "PROJ isea x_0=1",
        "PROJ isea orient=pole orient=isea",
        "PROJ isea orinet=pole",
        "PROJ isea azi=30",
    ] {
        assert!(transformer(method).is_err(), "{method}");
    }
    // And as a PARAMETER.
    let r_c = wkt("PROJ isea").replace("PARAMETER[\"x_0\"", "PARAMETER[\"R_C\",1],PARAMETER[\"x_0\"");
    assert!(SpatialRef::parse(&r_c).and_then(|t| Transformer::new(&lonlat, &t)).is_err(), "{r_c}");
}

#[test]
fn esri_method_name_parameters_are_checked() {
    let wkt = |projection: &str| {
        format!(
            "PROJCS[\"unknown\",GEOGCS[\"GCS_unknown\",DATUM[\"D_unknown\",SPHEROID[\"unknown\",6371007.18091847,0.0]],\
             PRIMEM[\"Greenwich\",0.0],UNIT[\"Degree\",0.0174532925199433]],PROJECTION[\"{projection}\"],\
             PARAMETER[\"x_0\",0.0],PARAMETER[\"y_0\",0.0],UNIT[\"Meter\",1.0]]"
        )
    };
    let lonlat = SpatialRef::parse("+proj=longlat +R=6371007.18091847").unwrap();
    let transformer = |projection: &str| SpatialRef::parse(&wkt(projection)).and_then(|t| Transformer::new(&lonlat, &t));
    assert!(transformer("PROJ_isea_orient=pole").is_ok());
    assert!(transformer("PROJ_ISEA_orient=pole").is_ok());
    // As for WKT 2, and the numeric parameters come from their own PARAMETERs.
    for projection in [
        "PROJ_isea_proj=merc",
        "PROJ_isea_nadgrids=grid",
        "PROJ_isea_units=km",
        "PROJ_isea_resolution=5",
        "PROJ_isea_orient=pole_orient=isea",
        "PROJ_isea_orinet=pole",
    ] {
        assert!(transformer(projection).is_err(), "{projection}");
    }
}

#[test]
fn origin_after_lon_0_and_lat_0_decides_the_inverse() {
    // PROJ 9.8.1: `+lat_0=90 +lon_0=0` is the pole orientation's origin, so its inverse is
    // defined; `+orient=pole +lon_0=11.25` is neither origin, so it is not.
    let lonlat = SpatialRef::parse("+proj=longlat +R=1").unwrap();
    let (x, y) = (0.28895720253590557, 0.1948919583135193);
    for crs in ["+proj=isea +R=1 +lat_0=90 +lon_0=0", "+proj=isea +R=1 +orient=pole"] {
        let target = SpatialRef::parse(crs).unwrap();
        let p = Transformer::new(&lonlat, &target).unwrap().transform(17.0, 11.0).unwrap();
        assert!((p.0 - x).abs() < 1e-12 && (p.1 - y).abs() < 1e-12, "{crs} {p:?}");
        let q = Transformer::new(&target, &lonlat).unwrap().transform(x, y).unwrap();
        assert!((q.0 - 17.0).abs() < 1e-9 && (q.1 - 11.0).abs() < 1e-9, "{crs} {q:?}");
    }
    let target = SpatialRef::parse("+proj=isea +R=1 +orient=pole +lon_0=11.25").unwrap();
    assert!(Transformer::new(&target, &lonlat).and_then(|t| t.transform(x, y)).is_err());
}
