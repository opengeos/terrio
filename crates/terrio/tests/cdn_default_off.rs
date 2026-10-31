//! The PROJ CDN is off by default, as in PROJ (network access is off unless `PROJ_NETWORK=ON`):
//! with neither `TERRIO_NETWORK` nor `PROJ_NETWORK` set and no `set_network_enabled` call, a
//! transformation whose best operation needs a grid the machine lacks sends no request and uses
//! the operation PROJ uses without the grid. A test binary of its own, so that no other test
//! changes the setting in its process.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use terrio::crs::{SpatialRef, Transformer};

/// A stand-in for the CDN on a local port: answers every request with a 404 and counts them.
fn fake_cdn() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = requests.clone();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            counter.fetch_add(1, Ordering::SeqCst);
            let mut request = [0u8; 8192];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });
    (url, requests)
}

/// NAD27 to NAD83 at (-100, 40): PROJ's best operation is NADCON's grid (`us_noaa_conus.tif`).
fn nad27_to_nad83() -> terrio::Result<(f64, f64)> {
    Transformer::new(&SpatialRef::parse("EPSG:4267")?, &SpatialRef::parse("EPSG:4269")?)?.transform(-100.0, 40.0)
}

#[test]
fn the_proj_cdn_is_off_by_default() {
    for var in ["TERRIO_NETWORK", "PROJ_NETWORK"] {
        assert!(std::env::var_os(var).is_none(), "{var} is set: this test checks the default");
    }
    let (endpoint, requests) = fake_cdn();
    // No local grids: only the CDN, the fake one, could provide the grid.
    terrio::crs::set_grid_search_paths(Some(Vec::new()));
    terrio::config::push_scope(HashMap::from([("TERRIO_CDN_ENDPOINT".to_string(), Some(endpoint))]));

    let (x, y) = nad27_to_nad83().unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), 0, "requests sent to the CDN by default");
    // PROJ 9 (rasterio 1.5.0, GDAL 3.12.1) with `PROJ_NETWORK=OFF` and no grid.
    let proj = (-100.00041558862637, 39.999996883362);
    assert!((x - proj.0).abs() < 1e-9 && (y - proj.1).abs() < 1e-9, "({x}, {y}), PROJ without the grid {proj:?}");

    // With the network on, the same transformation asks the CDN for the grid: the count above
    // would see a request.
    terrio::crs::set_network_enabled(Some(true));
    let _ = nad27_to_nad83();
    assert!(requests.load(Ordering::SeqCst) > 0, "no request with the network on");
    terrio::crs::set_network_enabled(None);
    terrio::config::pop_scope();
}
