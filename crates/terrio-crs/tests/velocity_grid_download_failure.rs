//! A velocity grid that counts as available (the network is on) but cannot be downloaded: the
//! point motion fails as a missing grid, which `transform_slice_or_nan` returns rather than marking
//! the point NaN. Its own test binary: the grid transport and network setting are process-wide.
#![cfg(all(feature = "grid-transport", not(target_family = "wasm")))]

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
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
    let mut xy = vec![(-75.0, 45.0)];
    assert!(matches!(t.transform_slice_or_nan(&mut xy), Err(Error::MissingGrid(_))), "{xy:?}");
    // The download was attempted and failed, rather than skipped before reaching the transport.
    assert!(attempts.load(Ordering::SeqCst) > 0);
    assert_eq!(xy, vec![(-75.0, 45.0)]);
}
