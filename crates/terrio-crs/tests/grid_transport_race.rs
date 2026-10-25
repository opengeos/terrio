//! An old transport may complete after replacement without poisoning the new cache.
#![cfg(all(feature = "grid-transport", not(target_family = "wasm")))]

use std::{
    io::Cursor,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};
use terrio_crs::{
    SpatialRef, Transformer,
    network::{self, GridStream, GridTransport},
};

struct Blocked {
    started: mpsc::SyncSender<()>,
    release: Mutex<mpsc::Receiver<()>>,
    data: Option<Vec<u8>>,
}
impl GridTransport for Blocked {
    fn open(&self, _: &str) -> std::io::Result<Box<dyn GridStream>> {
        self.started.send(()).unwrap();
        self.release.lock().unwrap().recv_timeout(Duration::from_secs(10)).unwrap();
        match &self.data {
            Some(data) => Ok(Box::new(Cursor::new(data.clone()))),
            None => Err(std::io::ErrorKind::NotFound.into()),
        }
    }
}

struct Missing(Arc<AtomicUsize>);
impl GridTransport for Missing {
    fn open(&self, _: &str) -> std::io::Result<Box<dyn GridStream>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(std::io::ErrorKind::NotFound.into())
    }
}

fn geoid() -> Vec<u8> {
    use tiff::{
        encoder::{TiffEncoder, colortype::Gray32Float},
        tags::Tag,
    };
    let mut bytes = Cursor::new(Vec::new());
    {
        let mut encoder = TiffEncoder::new(&mut bytes).unwrap();
        let mut image = encoder.new_image::<Gray32Float>(2, 2).unwrap();
        image.encoder().write_tag(Tag::ModelPixelScaleTag, &[1.0_f64, 1.0, 0.0][..]).unwrap();
        image.encoder().write_tag(Tag::ModelTiepointTag, &[0.0_f64, 0.0, 0.0, 0.0, 1.0, 0.0][..]).unwrap();
        image.write_data(&[10.0_f32; 4]).unwrap();
    }
    bytes.into_inner()
}

#[test]
fn in_flight_transport_results_cannot_restore_old_cache_entries() {
    terrio_crs::set_grid_search_paths(Some(Vec::new()));
    terrio_crs::set_network_enabled(Some(true));
    let source = SpatialRef::parse("+proj=longlat +datum=WGS84 +geoidgrids=terrio_transport_race.tif").unwrap();
    let target = SpatialRef::from_epsg(4979).unwrap();
    let query = || Transformer::new(&source, &target).unwrap().transform_3d(0.5, 0.5, 0.0);
    // Test both an old miss and an old valid grid/decoded grid set.
    for data in [None, Some(geoid())] {
        let valid = data.is_some();
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        network::set_grid_transport(Arc::new(Blocked { started: started_tx, release: Mutex::new(release_rx), data }));
        std::thread::scope(|scope| {
            let pending = scope.spawn(query);
            started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            let attempts = Arc::new(AtomicUsize::new(0));
            network::set_grid_transport(Arc::new(Missing(attempts.clone())));
            // Populate the new transport's negative cache before the old result
            // finishes. A late old result must not evict this newer cache entry.
            assert!(query().is_err());
            let cached_attempts = attempts.load(Ordering::SeqCst);
            assert!(cached_attempts > 0);
            release_tx.send(()).unwrap();
            let old = pending.join().unwrap();
            assert_eq!(old.is_ok(), valid, "old lookup: {old:?}");
            if let Ok((_, _, height)) = old {
                assert_eq!(height, 10.0);
            }
            assert!(query().is_err(), "old grid survived the transport replacement");
            assert_eq!(attempts.load(Ordering::SeqCst), cached_attempts, "old lookup evicted the new transport's cached result");
        });
    }
}
