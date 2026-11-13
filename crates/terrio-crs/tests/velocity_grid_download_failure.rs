//! A velocity grid that counts as available (the network is on) but cannot be downloaded: the
//! point motion fails as a missing grid, not as a point outside the grid, and
//! `transform_slice_or_nan` marks the point as it marks every point that fails. Its own test binary:
//! the grid transport and network setting are process-wide.
#![cfg(all(feature = "grid-transport", not(target_family = "wasm")))]

use std::{
    io::Cursor,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use terrio_crs::{
    Error, SpatialRef, TransformOptions, Transformer,
    network::{self, GridStream, GridTransport},
};

struct Unreachable(Arc<AtomicUsize>);
impl GridTransport for Unreachable {
    fn open(&self, _: &str) -> std::io::Result<Box<dyn GridStream>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(std::io::ErrorKind::NotFound.into())
    }
}

/// A velocity grid of 2 × 2 nodes 10° apart, read as pixel-is-point (no GeoKey directory), so
/// from 80°W to 70°W and 40°N to 50°N, with no motion.
fn velocity_grid() -> Vec<u8> {
    use tiff::{
        encoder::{TiffEncoder, colortype::RGB32Float},
        tags::Tag,
    };
    let mut bytes = Cursor::new(Vec::new());
    {
        let mut encoder = TiffEncoder::new(&mut bytes).unwrap();
        let mut image = encoder.new_image::<RGB32Float>(2, 2).unwrap();
        image.encoder().write_tag(Tag::ModelPixelScaleTag, &[10.0_f64, 10.0, 0.0][..]).unwrap();
        image.encoder().write_tag(Tag::ModelTiepointTag, &[0.0_f64, 0.0, 0.0, -80.0, 50.0, 0.0][..]).unwrap();
        image.write_data(&[0.0_f32; 12]).unwrap();
    }
    bytes.into_inner()
}

#[test]
fn a_velocity_grid_that_fails_to_download_is_a_missing_grid() {
    terrio_crs::set_grid_search_paths(Some(Vec::new()));
    terrio_crs::set_network_enabled(Some(true));
    let attempts = Arc::new(AtomicUsize::new(0));
    network::set_grid_transport(Arc::new(Unreachable(attempts.clone())));
    // NAD83(CSRS)v6 from epoch 2010 to 2020: the Canada velocity grid v6.
    let csrs = SpatialRef::from_epsg(8252).unwrap();
    let opts = TransformOptions { source_coordinate_epoch: Some(2010.0), target_coordinate_epoch: Some(2020.0), ..Default::default() };
    let t = Transformer::with_options(&csrs, &csrs, &opts).unwrap();
    assert!(matches!(t.transform(-75.0, 45.0), Err(Error::MissingGrid(_))));
    // The download was attempted and failed, rather than skipped before reaching the transport.
    assert!(attempts.load(Ordering::SeqCst) > 0);
    let mut xy = vec![(-75.0, 45.0)];
    assert_eq!(t.transform_slice_or_nan(&mut xy).unwrap(), 1);
    assert!(xy[0].0.is_nan() && xy[0].1.is_nan());
    // Registered afterwards, the grid is there: the failed download no longer counts, and a point
    // outside it fails as a point outside the domain.
    terrio_crs::register_grid("ca_nrc_NAD83v6VG.tif", velocity_grid());
    let (lon, lat) = t.transform(-75.0, 45.0).unwrap();
    assert!((lon + 75.0).abs() < 1e-9 && (lat - 45.0).abs() < 1e-9, "({lon}, {lat})");
    assert!(matches!(t.transform(-100.0, 40.0), Err(Error::Crs(_))));
}
