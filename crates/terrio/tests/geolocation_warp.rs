use terrio::ops::geolocation::GeolocationTransformer;
use terrio::ops::warp::{WarpOptions, warp};
use terrio::raster::RasterData;
use terrio::{DataType, Geolocation, Raster, RasterConfig, RasterDataset};
fn array(w: usize, h: usize, data: Vec<f64>) -> RasterDataset {
    RasterDataset::new(
        Raster::from_data_native(
            RasterConfig { cols: w, rows: h, bands: 1, data_type: DataType::F64, ..Default::default() },
            RasterData::F64(data),
        )
        .unwrap(),
        false,
    )
}
fn metadata(center: bool, step: f64) -> Geolocation {
    Geolocation {
        srs: "EPSG:4326".into(),
        x_dataset: String::new(),
        y_dataset: String::new(),
        x_band: 1,
        y_band: 1,
        pixel_offset: 0.0,
        line_offset: 0.0,
        pixel_step: step,
        line_step: step,
        pixel_center: center,
    }
}
#[test]
fn center_subsampling_extrapolation_and_dateline() {
    let x = array(3, 3, vec![179.0, -179.0, -177.0, 179.2, -178.8, -176.8, 179.4, -178.6, -176.6]);
    let y = array(3, 3, vec![10.0, 10.1, 10.2, 8.0, 8.1, 8.2, 6.0, 6.1, 6.2]);
    for center in [false, true] {
        for step in [1.0, 2.0] {
            let t = GeolocationTransformer::new(&metadata(center, step), &x, &y, 6, 6).unwrap();
            for (p, l) in [(0.0, 0.0), (0.5, 0.5), (2.0, 1.3), (5.5, 5.5)] {
                let (a, b) = t.to_geo(p, l).unwrap();
                let (u, v) = t.to_pixel(a, b).unwrap();
                assert!((u - p).abs() < 1e-7 && (v - l).abs() < 1e-7, "{u} {v} != {p} {l}");
                let (u, v) = t.to_pixel(a - 360.0, b).unwrap();
                assert!((u - p).abs() < 1e-7 && (v - l).abs() < 1e-7);
            }
        }
    }
}
#[test]
fn masks_invalid_coordinates_and_rejects_bad_metadata() {
    let x = array(3, 2, vec![0.0, 1.0, f64::NAN, 0.0, 1.0, 2.0]);
    let y = array(3, 2, vec![1.0, 1.0, 1.0, 0.0, 0.0, 0.0]);
    let t = GeolocationTransformer::new(&metadata(false, 1.0), &x, &y, 3, 2).unwrap();
    assert!(t.to_pixel(0.5, 0.5).is_some());
    assert!(t.to_geo(1.5, 0.5).is_none());
    assert!(t.to_pixel(1.5, 1.0).is_none());
    assert!(GeolocationTransformer::new(&metadata(false, 0.0), &x, &y, 3, 2).is_err());
}
#[test]
fn identity_warp_from_center_arrays_and_missing_arrays_error() {
    let source = array(4, 3, (1..=12).map(f64::from).collect());
    let x = array(2, 2, vec![11.0, 13.0, 11.0, 13.0]);
    let y = array(2, 2, vec![49.0, 49.0, 47.0, 47.0]);
    let t = GeolocationTransformer::new(&metadata(true, 2.0), &x, &y, 4, 3).unwrap();
    let opts = WarpOptions {
        geolocation: Some(t),
        dst_transform: Some([10.0, 1.0, 0.0, 50.0, 0.0, -1.0]),
        size: Some((4, 3)),
        error_threshold: 0.0,
        ..Default::default()
    };
    let out = warp(&[&source], &opts).unwrap();
    assert_eq!(out.raster.band_view(0).as_slice(), source.raster.band_view(0).as_slice());
    assert!(warp(&[&source], &WarpOptions { geoloc: true, ..Default::default() }).is_err());
}

#[test]
fn invalid_first_node_does_not_shift_valid_longitudes() {
    let mut x = array(3, 2, vec![1e30, 10.0, 11.0, 9.0, 10.0, 11.0]);
    let y = array(3, 2, vec![1.0, 1.0, 1.0, 0.0, 0.0, 0.0]);
    for nodata in [false, true] {
        x.has_nodata = nodata;
        x.raster.nodata = 1e30;
        let t = GeolocationTransformer::new(&metadata(false, 1.0), &x, &y, 3, 2).unwrap();
        let p = t.to_pixel(10.5, 0.5).unwrap();
        assert!((p.0 - 1.5).abs() < 1e-8 && (p.1 - 0.5).abs() < 1e-8);
        assert!(t.to_geo(0.5, 0.5).is_none());
    }
}
