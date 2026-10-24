//! Coordinate-operation policy regressions from #257.
use terrio::crs::{SpatialRef, TransformOptions, Transformer};

#[test]
fn ballpark_can_be_refused() {
    let src = SpatialRef::parse("EPSG:4610").unwrap();
    let dst = SpatialRef::parse("EPSG:4326").unwrap();
    let t = Transformer::new(&src, &dst).unwrap();
    assert_eq!(t.transform(110.0, 35.0).unwrap(), (110.0, 35.0));
    drop(t);
    assert!(terrio::warnings::take().iter().any(|w| w.contains("ballpark")));
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
}

#[test]
fn operation_options_are_validated() {
    assert!(TransformOptions::from_options(&[("ALLOW_BALLPARK", "perhaps")]).is_err());
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

    drop(t);
    terrio::warnings::take();
    let t = Transformer::new(&SpatialRef::parse("EPSG:4326").unwrap(), &dst).unwrap();
    assert!(t.transform(11.0, 46.5).is_ok());
    drop(t);
    assert!(terrio::warnings::take().iter().any(|w| w.contains("outside its area")));
}
