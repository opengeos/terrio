//! Transport changes invalidate negative grid lookups; installing a default preserves overrides.
#![cfg(all(feature = "grid-transport", not(target_family = "wasm")))]

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use terrio_crs::{
    SpatialRef, Transformer,
    network::{self, GridStream, GridTransport},
};

struct Missing(Arc<AtomicUsize>);
impl GridTransport for Missing {
    fn open(&self, _: &str) -> std::io::Result<Box<dyn GridStream>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(std::io::ErrorKind::NotFound.into())
    }
}

#[test]
fn transport_replacement_retries_missing_grids() {
    terrio_crs::set_grid_search_paths(Some(Vec::new()));
    terrio_crs::set_network_enabled(Some(true));
    let source = SpatialRef::parse("+proj=longlat +datum=WGS84 +geoidgrids=terrio_missing_transport_test.tif").unwrap();
    let target = SpatialRef::from_epsg(4979).unwrap();
    let query = || {
        let transform = Transformer::new(&source, &target).unwrap();
        assert!(transform.transform_3d(0.0, 0.0, 0.0).is_err());
    };
    let first = Arc::new(AtomicUsize::new(0));
    network::set_grid_transport(Arc::new(Missing(first.clone())));
    query();
    let attempts = first.load(Ordering::SeqCst);
    assert!(attempts > 0);
    query();
    assert_eq!(first.load(Ordering::SeqCst), attempts);
    let replacement = Arc::new(AtomicUsize::new(0));
    network::set_default_grid_transport(Arc::new(Missing(replacement.clone())));
    query();
    assert_eq!(replacement.load(Ordering::SeqCst), 0);
    assert_eq!(first.load(Ordering::SeqCst), attempts);
    network::set_grid_transport(Arc::new(Missing(replacement.clone())));
    query();
    assert!(replacement.load(Ordering::SeqCst) > 0);
}
