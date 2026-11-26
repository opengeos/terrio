//! Strict refusals distinguish an unavailable required operation from no permitted operation.
//! MissingGrid includes missing grid data and insufficient coverage for a point.

use terrio_crs::{Error, SpatialRef, TransformOptions, Transformer};

#[test]
fn strict_refusals_have_their_own_variants() {
    // No installed or downloadable grids in this test executable.
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));

    // CH1903 to CH1903+: the best operation needs a grid (GDAL's ONLY_BEST=YES refuses it).
    let (src, dst) = (SpatialRef::parse("EPSG:21781").unwrap(), SpatialRef::parse("EPSG:2056").unwrap());
    let only_best = TransformOptions { only_best: true, ..Default::default() };
    let t = Transformer::with_options(&src, &dst, &only_best).unwrap();
    let e = t.transform(600000.0, 200000.0).unwrap_err();
    assert!(matches!(e, Error::MissingGrid(_)), "{e:?}");
    assert!(matches!(t.check_policy(), Err(Error::MissingGrid(_))));
    // The message is the same as before.
    assert!(e.to_string().starts_with("coordinate reference system: ONLY_BEST=YES"), "{e}");

    // Tokyo to WGS 84 has only a ballpark operation (GDAL's ALLOW_BALLPARK=NO refuses it).
    let (src, dst) = (SpatialRef::parse("EPSG:4610").unwrap(), SpatialRef::parse("EPSG:4326").unwrap());
    let no_ballpark = TransformOptions { allow_ballpark: Some(false), ..Default::default() };
    let e = Transformer::with_options(&src, &dst, &no_ballpark).err();
    assert!(matches!(e, Some(Error::NoOperation(_))), "{e:?}");

    // WGS 84 + EGM96 height to WGS 84 3D without the geoid grid: the same two kinds.
    let (src, dst) = (SpatialRef::parse("EPSG:4326+5773").unwrap(), SpatialRef::parse("EPSG:4979").unwrap());
    let t = Transformer::with_options(&src, &dst, &only_best).unwrap();
    assert!(matches!(t.transform_3d(10.0, 50.0, 100.0), Err(Error::MissingGrid(_))));
    let t = Transformer::with_options(&src, &dst, &no_ballpark).unwrap();
    assert!(matches!(t.transform_3d(10.0, 50.0, 100.0), Err(Error::NoOperation(_))));

    // A definition error stays `Crs`.
    assert!(matches!(SpatialRef::parse("+proj=nonsense"), Err(Error::Crs(_))));
}

#[test]
fn strict_policy_allows_vertical_unit_conversion() {
    // GDAL 3.12.2 / PROJ 9.8.1, traditional GIS order, no grids:
    // NAVD88 metres to NAVD88 US survey feet is a unit conversion, not a ballpark.
    let source = SpatialRef::parse("EPSG:4326+5703").unwrap();
    let target = SpatialRef::parse("EPSG:4326+6360").unwrap();
    let options = TransformOptions { only_best: true, allow_ballpark: Some(false), ..Default::default() };
    let forward = Transformer::with_options(&source, &target, &options).unwrap();
    let actual = forward.transform_3d(-100.0, 40.0, 100.0).unwrap();
    assert_eq!((actual.0, actual.1), (-100.0, 40.0));
    assert!((actual.2 - 328.08333333333337).abs() < 1e-10);
    let inverse = Transformer::with_options(&target, &source, &options).unwrap();
    let back = inverse.transform_3d(actual.0, actual.1, actual.2).unwrap();
    assert!((back.2 - 100.0).abs() < 1e-10);
}

#[test]
fn vertical_crs_in_feet_refuses_a_missing_geoid_grid_as_in_metres() {
    // No installed or downloadable grids in this test executable.
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    // NAVD88 height (ftUS) uses NAVD88 height's GEOID18: without its grid, the same refusals as
    // NAVD88 height in metres, and by default heights kept (PROJ's ballpark), the unit converted.
    let only_best = TransformOptions { only_best: true, ..Default::default() };
    let no_ballpark = TransformOptions { allow_ballpark: Some(false), ..Default::default() };
    let dst = SpatialRef::parse("EPSG:6319").unwrap();
    // PROJ 9.9.0 names the ballpark from the vertical CRS.
    for (src, z, vertical) in [
        ("EPSG:6318+5703", 100.0, "NAVD88 height"),
        ("EPSG:6318+6360", 100.0 / 0.3048006096012192, "NAVD88 height (ftUS)"),
        ("EPSG:6318+8228", 100.0 / 0.3048, "NAVD88 height (ft)"),
    ] {
        let src = SpatialRef::parse(src).unwrap();
        let t = Transformer::with_options(&src, &dst, &only_best).unwrap();
        assert!(matches!(t.transform_3d(-97.0, 30.0, z), Err(Error::MissingGrid(_))), "{src:?}");
        let t = Transformer::with_options(&src, &dst, &no_ballpark).unwrap();
        assert!(matches!(t.transform_3d(-97.0, 30.0, z), Err(Error::NoOperation(_))), "{src:?}");
        let t = Transformer::new(&src, &dst).unwrap();
        let (_, _, h) = t.transform_3d(-97.0, 30.0, z).unwrap();
        assert!((h - 100.0).abs() < 1e-9, "{h}");
        let op = t.operation_at(-97.0, 30.0, z).unwrap().unwrap();
        let name = format!(
            "Transformation from {vertical} to NAD83(2011) (ballpark vertical transformation, without ellipsoid height to vertical height correction)"
        );
        assert!(op.ballpark && op.name == name, "{op:?}");
    }
}

#[test]
fn geoid_model_without_a_grid_free_operation_refuses_a_missing_grid_as_proj() {
    // No installed or downloadable grids in this test executable.
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    // DKMSL(2022) depth (EPSG:10547) has one geoid model, ETRS89 to DKMSL(2022) depth
    // (dk_sdfi_dkmsl_2022.tif), and no operation without a grid. PROJ 9.9.0 (cs2cs, network off,
    // no grids) refuses it with --only-best (`Grid dk_sdfi_dkmsl_2022.tif is not available`) and
    // --no-ballpark, and by default keeps the height: depth 10 m at 56 N, 10.5 E is -10 m above
    // the ETRS89 ellipsoid, and back.
    let only_best = TransformOptions { only_best: true, ..Default::default() };
    let no_ballpark = TransformOptions { allow_ballpark: Some(false), ..Default::default() };
    for (a, b) in [("EPSG:4258+10547", "EPSG:4937"), ("EPSG:4937", "EPSG:4258+10547")] {
        let (src, dst) = (SpatialRef::parse(a).unwrap(), SpatialRef::parse(b).unwrap());
        let t = Transformer::with_options(&src, &dst, &only_best).unwrap();
        let r = t.transform_3d(10.5, 56.0, 10.0);
        assert!(matches!(r, Err(Error::MissingGrid(_))), "{a} to {b}: {r:?}");
        let r = Transformer::with_options(&src, &dst, &no_ballpark).and_then(|t| t.transform_3d(10.5, 56.0, 10.0));
        assert!(matches!(r, Err(Error::NoOperation(_))), "{a} to {b}: {r:?}");
        let (_, _, h) = Transformer::new(&src, &dst).unwrap().transform_3d(10.5, 56.0, 10.0).unwrap();
        assert!((h + 10.0).abs() < 1e-9, "{a} to {b}: {h}, PROJ -10");
    }
}

#[test]
fn global_latitude_of_origin_validation_matches_proj() {
    // GDAL 3.12.2 / PROJ 9.8.1 rejects these during CRS creation even
    // when the particular projection does not use an origin latitude.
    for projection in ["+proj=longlat", "+proj=utm +zone=33", "+proj=geocent", "+proj=webmerc", "+proj=ob_tran +o_proj=longlat +o_lat_p=45"]
    {
        for latitude in [-95, 95] {
            let definition = format!("{projection} +lat_0={latitude} +datum=WGS84");
            assert!(SpatialRef::parse(&definition).is_err(), "{definition}");
        }
    }
}
