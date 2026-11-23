//! Coordinate-operation policy regressions from #257.
use terrio::crs::{SpatialRef, TransformOptions, Transformer};

#[test]
fn ballpark_can_be_refused() {
    let src = SpatialRef::parse("EPSG:4610").unwrap();
    let dst = SpatialRef::parse("EPSG:4326").unwrap();
    let t = Transformer::new(&src, &dst).unwrap();
    assert_eq!(t.transform(110.0, 35.0).unwrap(), (110.0, 35.0));
    t.flush_warnings();
    assert!(terrio::warnings::take().iter().any(|w| w.contains("ballpark")));
    drop(t);
    assert!(terrio::warnings::take().is_empty());
    let opts = TransformOptions { allow_ballpark: Some(false), ..Default::default() };
    assert!(Transformer::with_options(&src, &dst, &opts).is_err());
    // ONLY_BEST by itself permits a ballpark when there is no better operation.
    let opts = TransformOptions { only_best: true, ..Default::default() };
    assert!(Transformer::with_options(&src, &dst, &opts).unwrap().transform(110.0, 35.0).is_ok());
}

#[test]
fn same_datum_is_not_a_ballpark() {
    let src = SpatialRef::parse("EPSG:4326").unwrap();
    let dst = SpatialRef::parse("EPSG:3857").unwrap();
    let opts = TransformOptions { allow_ballpark: Some(false), only_best: true, ..Default::default() };
    assert!(Transformer::with_options(&src, &dst, &opts).unwrap().transform(10.0, 45.0).is_ok());
    // A custom WGS84 side uses the direct WGS84 route, not an empty hub leg.
    let custom = SpatialRef::parse("+proj=longlat +datum=WGS84").unwrap();
    let osgb = SpatialRef::parse("EPSG:4277").unwrap();
    let options = TransformOptions { allow_ballpark: Some(false), ..Default::default() };
    assert!(Transformer::with_options(&custom, &osgb, &options).unwrap().transform(-2.0, 52.0).is_ok());
}

#[test]
fn operation_options_are_validated() {
    assert!(TransformOptions::from_options(&[("ALLOW_BALLPARK", "perhaps")]).is_err());
    assert!(TransformOptions::from_strict_options(&[("ALLOW_BALPARK", "NO")]).is_err());
    assert!(TransformOptions::from_options(&[("UNRELATED_TRANSFORMER_OPTION", "YES")]).is_ok());
    let opts = TransformOptions::from_options(&[("allow_ballpark", "no"), ("ONLY_BEST", "yes")]).unwrap();
    assert_eq!(opts.allow_ballpark, Some(false));
    assert!(opts.only_best);
}

#[test]
fn missing_best_grid_can_be_refused_and_outside_area_is_reported() {
    // Confine this test executable to a machine with no installed grids.
    terrio::crs::set_grid_search_paths(Some(vec![]));
    terrio::crs::set_network_enabled(Some(false));
    let src = SpatialRef::parse("EPSG:21781").unwrap();
    let dst = SpatialRef::parse("EPSG:2056").unwrap();
    let t = Transformer::new(&src, &dst).unwrap();
    assert!(t.transform(600000.0, 200000.0).is_ok());
    drop(t);
    assert!(terrio::warnings::take().iter().any(|w| w.contains("missing grid")));
    let opts = TransformOptions { only_best: true, ..Default::default() };
    let t = Transformer::with_options(&src, &dst, &opts).unwrap();
    let error = t.transform(600000.0, 200000.0).unwrap_err().to_string();
    assert!(error.contains("ONLY_BEST"), "{error}");
    // An outside-area point chooses a grid-free fallback. The previous missing-grid
    // diagnostic must not turn that independently valid point into a failure.
    assert!(t.transform(0.0, 0.0).is_ok());
    assert!(t.check_policy().is_err());
    let raster = wbraster::Raster::from_data(
        wbraster::RasterConfig {
            cols: 4,
            rows: 4,
            bands: 1,
            x_min: 600000.0,
            y_min: 200000.0,
            cell_size: 10.0,
            crs: wbraster::CrsInfo::from_epsg(21781),
            ..Default::default()
        },
        vec![1.0; 16],
    )
    .unwrap();
    let source = terrio::RasterDataset::new(raster, false);
    let options = terrio::ops::warp::WarpOptions {
        dst_srs: Some(dst.clone()),
        transform_options: opts,
        extent: Some((2600000.0, 1200000.0, 2600040.0, 1200040.0)),
        size: Some((4, 4)),
        ..Default::default()
    };
    assert!(terrio::ops::warp::warp(&[&source], &options).is_err());
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input.tif");
    let output = dir.path().join("output.tif");
    terrio::write_raster_dataset(&source, input.to_str().unwrap(), &Default::default()).unwrap();
    assert!(
        terrio::ops::warp::warp_to_path(&[input.to_str().unwrap().into()], &options, output.to_str().unwrap(), &Default::default())
            .is_err()
    );
    assert!(!output.exists());

    // Every preflight boundary sample is grid-free, but later interior rows cross
    // Switzerland. A failure after opening the streamed writer must remove its output.
    let boundary = Transformer::with_options(&dst, &src, &options.transform_options).unwrap();
    for i in 0..=20 {
        let f = i as f64 / 20.0;
        for (x, y) in [(1e6 + f * 3e6, 0.0), (1e6 + f * 3e6, 3e6), (1e6, f * 3e6), (4e6, f * 3e6)] {
            assert!(boundary.transform(x, y).is_ok());
        }
    }
    assert!(boundary.check_policy().is_ok());
    assert!(boundary.transform(2600000.0, 1200000.0).is_err());
    drop(boundary);
    let wide = terrio::RasterDataset::new(
        wbraster::Raster::from_data(
            wbraster::RasterConfig {
                cols: 8,
                rows: 8,
                bands: 1,
                x_min: -1e6,
                y_min: -1e6,
                cell_size: 375000.0,
                crs: wbraster::CrsInfo::from_epsg(21781),
                ..Default::default()
            },
            vec![1.0; 64],
        )
        .unwrap(),
        false,
    );
    let late_input = dir.path().join("late-input.tif");
    let late_output = dir.path().join("late-output.tif");
    terrio::write_raster_dataset(&wide, late_input.to_str().unwrap(), &Default::default()).unwrap();
    let late_options = terrio::ops::warp::WarpOptions {
        extent: Some((1e6, 0.0, 4e6, 3e6)),
        size: Some((100, 100)),
        error_threshold: 0.0,
        ..options.clone()
    };
    let error = terrio::ops::warp::warp_to_path(
        &[late_input.to_str().unwrap().into()],
        &late_options,
        late_output.to_str().unwrap(),
        &Default::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("ONLY_BEST"));
    assert!(!late_output.exists());
    assert!(!dir.path().join("late-output.tif.aux.xml").exists());

    // The source and target share a CRS; only the extent/cutline need a datum shift.
    // Their transforms must still enforce ONLY_BEST.
    let mut target_source = source.clone();
    target_source.raster.crs = wbraster::CrsInfo::from_epsg(2056);
    let extent_options = terrio::ops::warp::WarpOptions {
        dst_srs: Some(dst.clone()),
        transform_options: options.transform_options.clone(),
        extent_srs: Some(src.clone()),
        extent: Some((600000.0, 200000.0, 600040.0, 200040.0)),
        size: Some((4, 4)),
        ..Default::default()
    };
    assert!(terrio::ops::warp::warp(&[&target_source], &extent_options).unwrap_err().to_string().contains("ONLY_BEST"));
    let cutline = terrio::ops::warp::Cutline {
        polygons: vec![vec![vec![(600000.0, 200000.0), (600040.0, 200000.0), (600040.0, 200040.0), (600000.0, 200000.0)]]],
        srs: Some(src.clone()),
    };
    let cut_options =
        terrio::ops::warp::WarpOptions { cutline: Some(cutline), crop_to_cutline: true, extent_srs: None, extent: None, ..extent_options };
    assert!(terrio::ops::warp::warp(&[&target_source], &cut_options).unwrap_err().to_string().contains("ONLY_BEST"));

    drop(t);
    terrio::warnings::take();
    let t = Transformer::new(&SpatialRef::parse("EPSG:4326").unwrap(), &dst).unwrap();
    assert!(t.transform(11.0, 46.5).is_ok());
    drop(t);
    assert!(terrio::warnings::take().iter().any(|w| w.contains("outside its area")));
}

#[test]
fn out_of_domain_points_do_not_poison_strict_transformers() {
    let src = SpatialRef::parse("EPSG:4326").unwrap();
    let dst = SpatialRef::parse("EPSG:3857").unwrap();
    let opts = TransformOptions { only_best: true, allow_ballpark: Some(false), ..Default::default() };
    let t = Transformer::with_options(&src, &dst, &opts).unwrap();
    assert!(t.transform(0.0, 100.0).is_err());
    assert!(t.transform(0.0, 45.0).is_ok());
    assert!(t.check_policy().is_ok());
}

/// Into a CRS bound to WGS 84 by `+towgs84`, PROJ chains each OSGB36 transformation with the
/// shift; with ALLOW_BALLPARK=NO, a point outside all their areas goes through the first that
/// needs no grid, as in PROJ 9.9.0 and GDAL 3.13.3 (`OGRCoordinateTransformationOptions::
/// SetBallparkAllowed(false)`), not refused, and later points are unaffected.
#[test]
fn strict_hub_routes_fall_back_as_proj_does_without_poisoning_later_points() {
    terrio::crs::set_grid_search_paths(Some(vec![]));
    terrio::crs::set_network_enabled(Some(false));
    let src = SpatialRef::parse("EPSG:4277").unwrap();
    let dst = SpatialRef::parse("+proj=longlat +a=6371000 +b=6371000 +towgs84=1,2,3").unwrap();
    let options = TransformOptions { allow_ballpark: Some(false), ..Default::default() };
    let t = Transformer::with_options(&src, &dst, &options).unwrap();
    for ((x, y), (lon, lat)) in
        [((140.0, 30.0), (139.99840532938242, 29.839174094869808)), ((-2.0, 52.0), (-2.001456340682326, 51.81356323348634))]
    {
        let (a, b) = t.transform(x, y).unwrap();
        assert!((a - lon).abs() < 1e-8 && (b - lat).abs() < 1e-8, "({x}, {y}): ({a}, {b}), PROJ ({lon}, {lat})");
    }
    assert!(t.check_policy().is_ok());
}

#[test]
fn missing_geoid_models_can_be_refused() {
    // As GDAL 3.13.3 / PROJ 9.9 without grids: heights are kept (PROJ's ballpark vertical
    // transformation), unless ONLY_BEST=YES (the EGM96 or NLGEO2018 grid is missing) or
    // ALLOW_BALLPARK=NO refuses it.
    terrio::crs::set_grid_search_paths(Some(vec![]));
    terrio::crs::set_network_enabled(Some(false));
    let cases = [
        ("EPSG:4326+5773", "EPSG:4979", (153.0, -27.0, 0.0)),
        ("EPSG:4979", "EPSG:4326+5773", (153.0, -27.0, 0.0)),
        ("EPSG:4979", "EPSG:7415", (5.0, 52.0, 0.0)),
        ("EPSG:4326+5773", "EPSG:4326+3855", (153.0, -27.0, 0.0)),
    ];
    for (src, dst, (x, y, z)) in cases {
        let (src, dst) = (SpatialRef::parse(src).unwrap(), SpatialRef::parse(dst).unwrap());
        let (_, _, h) = Transformer::new(&src, &dst).unwrap().transform_3d(x, y, z).unwrap();
        assert_eq!(h, 0.0);
        for (opts, refusal) in [
            (TransformOptions { only_best: true, ..Default::default() }, "ONLY_BEST"),
            (TransformOptions { allow_ballpark: Some(false), ..Default::default() }, "ALLOW_BALLPARK"),
        ] {
            let result = Transformer::with_options(&src, &dst, &opts).and_then(|t| t.transform_3d(x, y, z));
            let error = result.expect_err(&format!("{refusal}: {:?} -> {:?}", src.name(), dst.name())).to_string();
            assert!(error.contains(refusal), "{src:?} -> {dst:?}: {error}");
        }
    }
    // A vertical CRS kept as it is, or without a height on either side, is no vertical operation.
    let opts = TransformOptions { only_best: true, allow_ballpark: Some(false), ..Default::default() };
    let same = SpatialRef::parse("EPSG:4326+5773").unwrap();
    assert!(Transformer::with_options(&same, &same, &opts).unwrap().transform_3d(153.0, -27.0, 5.0).is_ok());
    let flat = (SpatialRef::parse("EPSG:4326").unwrap(), SpatialRef::parse("EPSG:3857").unwrap());
    assert!(Transformer::with_options(&flat.0, &flat.1, &opts).unwrap().transform_3d(10.0, 45.0, 5.0).is_ok());
}

#[test]
fn promoted_vector_heights_match_gdal_and_keep_geometry_dimensions() {
    // GDAL 3.12.2 / PROJ 9.8.1, OGR Geometry.Transform with a promoted RD New CRS.
    let json = br#"{"type":"FeatureCollection","features":[
        {"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[85000,447000,0]}},
        {"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[85000,447000,100]}},
        {"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[85000,447000]}}
    ]}"#;
    let mut layer = terrio::memory::parse_geojson_layer(json, "points").unwrap();
    SpatialRef::from_epsg(28992).unwrap().apply_to_layer(&mut layer);
    let target = SpatialRef::from_epsg(4979).unwrap();
    for promote in [false, true] {
        let options = TransformOptions { promote_to_3d: promote, ..Default::default() };
        let output = terrio::vector::reproject_layer_with_options(layer.clone(), &target, &options).unwrap();
        let expected = [
            (4.367683424792349, 52.006949098822965, Some(if promote { 43.57639261614531 } else { 0.0 })),
            (
                if promote { 4.36768342318245 } else { 4.367683424792349 },
                if promote { 52.0069491123924 } else { 52.006949098822965 },
                Some(if promote { 143.576799856499 } else { 100.0 }),
            ),
            (4.367683424792349, 52.006949098822965, None),
        ];
        assert_eq!(output.features.len(), expected.len());
        for (feature, (x, y, z)) in output.features.iter().zip(expected) {
            let Some(terrio::Geometry::Point(point)) = &feature.geometry else { panic!("expected a point") };
            assert!((point.x - x).abs() < 1e-8 && (point.y - y).abs() < 1e-8);
            match (point.z, z) {
                (Some(got), Some(want)) => assert!((got - want).abs() < 0.001, "promote={promote}: {got} vs {want}"),
                (None, None) => {}
                other => panic!("geometry dimensions changed: {other:?}"),
            }
        }
    }
}
