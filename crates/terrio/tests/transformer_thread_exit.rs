//! A `Transformer` dropped while its thread's locals are destroyed (held in a caller's
//! `thread_local!` cache) reports its warnings without touching the already destroyed
//! thread-local warning list, rather than aborting the process.

use std::cell::RefCell;
use terrio::crs::{SpatialRef, Transformer};

thread_local! {
    static CACHE: RefCell<Option<Transformer>> = const { RefCell::new(None) };
}

#[test]
fn a_transformer_in_a_thread_local_cache_is_dropped_at_thread_exit() {
    std::thread::spawn(|| {
        // A ballpark operation between different ellipsoids, whose warning is pending until the
        // transformer reports it.
        let src = SpatialRef::parse("+proj=longlat +ellps=bessel").unwrap();
        let dst = SpatialRef::from_epsg(4326).unwrap();
        let t = Transformer::new(&src, &dst).unwrap();
        t.transform(10.0, 50.0).unwrap();
        CACHE.with(|c| *c.borrow_mut() = Some(t));
        // The warning list is set up after the cache, so it is destroyed first.
        let _ = terrio::warnings::take();
    })
    .join()
    .unwrap();
}
