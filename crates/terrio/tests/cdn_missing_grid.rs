//! With the network on and the grid of the best transformation missing on the CDN (HTTP 404), a
//! transformation fails as PROJ's does, rather than falling back on PROJ's ballpark: rasterio
//! 1.5.0 (GDAL 3.12.1, PROJ 9.7.1) raises "Cannot open .../us_noaa_nadcon5_nad27_nad83_1986_conus.tif:
//! HTTP error 404" after one request, and pyproj 3.8.0 (PROJ 9.8.1) returns infinities. With the
//! network off, both use PROJ's operation without the grid. A test binary of its own, so that no
//! other test changes the network setting in its process; its tests take turns (`SERIAL`).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex, PoisonError};

use terrio::crs::{SpatialRef, Transformer};

/// The tests of this binary change the process's network setting: one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

/// A stand-in for the CDN on a local port: answers a range request with `range_status` and any
/// other with a 404, and keeps the paths, a range request's marked `(range)`.
fn fake_cdn(range_status: u16) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut request = [0u8; 8192];
            let n = stream.read(&mut request).unwrap_or(0);
            let text = String::from_utf8_lossy(&request[..n]).to_string();
            let path = text.lines().next().unwrap_or_default().split_whitespace().nth(1).unwrap_or_default().to_string();
            let ranged = text.lines().any(|l| l.to_ascii_lowercase().starts_with("range:"));
            seen.lock().unwrap().push(if ranged { format!("{path} (range)") } else { path });
            let status = if ranged { range_status } else { 404 };
            let reason = if status == 404 { "Not Found" } else { "Forbidden" };
            let _ = write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });
    (url, requests)
}

/// NAD27 to NAD83 at (-100, 40): PROJ's best operation needs NADCON5's grid.
fn nad27_to_nad83() -> terrio::Result<(f64, f64)> {
    Transformer::new(&SpatialRef::parse("EPSG:4267")?, &SpatialRef::parse("EPSG:4269")?)?.transform(-100.0, 40.0)
}

#[test]
fn a_grid_missing_on_the_cdn_fails_the_transformation_as_in_proj() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let (endpoint, requests) = fake_cdn(404);
    // No local grids: only the CDN, the fake one, could provide the grid.
    terrio::crs::set_grid_search_paths(Some(Vec::new()));
    terrio::config::push_scope(HashMap::from([("TERRIO_CDN_ENDPOINT".to_string(), Some(endpoint))]));

    // Network off: PROJ's operation without the grid (rasterio 1.5.0 and pyproj 3.8.0 alike).
    terrio::crs::set_network_enabled(Some(false));
    terrio::crs::reset_grid_cache();
    let (x, y) = nad27_to_nad83().unwrap();
    let proj = (-100.00041558862637, 39.999996883362);
    assert!((x - proj.0).abs() < 1e-9 && (y - proj.1).abs() < 1e-9, "({x}, {y}), PROJ without the grid {proj:?}");
    assert!(requests.lock().unwrap().is_empty());

    // Network on, the grid missing on the CDN: the point fails after asking for the grid PROJ
    // asks for, once; not the ballpark (-100, 40), nor another operation.
    terrio::crs::set_network_enabled(Some(true));
    terrio::crs::reset_grid_cache();
    let r = nad27_to_nad83();
    assert!(r.is_err(), "{r:?}: PROJ fails the point");
    assert_eq!(*requests.lock().unwrap(), ["/us_noaa_nadcon5_nad27_nad83_1986_conus.tif (range)"]);

    terrio::crs::set_network_enabled(None);
    terrio::crs::reset_grid_cache();
    terrio::config::pop_scope();
}

#[test]
fn a_grid_the_network_does_not_give_whole_fails_the_transformation_as_in_proj() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    // A server that forbids range requests (403) and does not have the file (404): terrio reads
    // the grid in place, then downloads it (as with TERRIO_GRID_DOWNLOAD=YES), and fails the
    // point. The download goes by range too, as GDAL's /vsicurl/ downloads a file
    // (`http::download`), so the server forbids it as well. PROJ fails the point on the 403:
    // rasterio 1.5.0 raises "Cannot open .../us_noaa_nadcon5_nad27_nad83_1986_conus.tif: HTTP
    // error 403" after one request, and pyproj 3.8.0 returns infinities.
    let (endpoint, requests) = fake_cdn(403);
    let cache = tempfile::tempdir().unwrap();
    // SAFETY: the tests of this binary take turns (`SERIAL`), and no other thread reads the
    // environment meanwhile.
    unsafe { std::env::set_var("TERRIO_GRID_CACHE", cache.path()) };
    terrio::crs::set_grid_search_paths(Some(Vec::new()));
    terrio::config::push_scope(HashMap::from([("TERRIO_CDN_ENDPOINT".to_string(), Some(endpoint))]));
    terrio::crs::set_network_enabled(Some(true));
    terrio::crs::reset_grid_cache();
    let r = nad27_to_nad83();
    assert!(r.is_err(), "{r:?}: PROJ fails the point");
    // The read in place, then the download's first request.
    let grid = "/us_noaa_nadcon5_nad27_nad83_1986_conus.tif (range)";
    assert_eq!(*requests.lock().unwrap(), [grid, grid]);
    assert_eq!(std::fs::read_dir(cache.path()).unwrap().count(), 0, "nothing kept of a failed download");

    terrio::crs::set_network_enabled(None);
    terrio::crs::reset_grid_cache();
    terrio::config::pop_scope();
}

#[test]
fn a_grid_missing_on_the_cdn_fails_a_chain_of_transformations_as_in_proj() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    // AGD84 to GDA2020 at (133, -25): terrio chains two transformations (a route of chains, not a
    // list), the best needing AGD84 -> GDA94's grid, which PROJ asks for too. rasterio 1.5.0
    // raises "Cannot open .../au_icsm_National_84_02_07_01.tif: HTTP error 404" after one
    // request, and pyproj 3.8.0 returns infinities; not another chain's (133.0013, -24.9985).
    let (endpoint, requests) = fake_cdn(404);
    terrio::crs::set_grid_search_paths(Some(Vec::new()));
    terrio::config::push_scope(HashMap::from([("TERRIO_CDN_ENDPOINT".to_string(), Some(endpoint))]));
    terrio::crs::set_network_enabled(Some(true));
    terrio::crs::reset_grid_cache();
    let agd84_to_gda2020 = || -> terrio::Result<(f64, f64)> {
        Transformer::new(&SpatialRef::parse("EPSG:4203")?, &SpatialRef::parse("EPSG:7844")?)?.transform(133.0, -25.0)
    };
    let r = agd84_to_gda2020();
    assert!(r.is_err(), "{r:?}: PROJ fails the point");
    assert_eq!(*requests.lock().unwrap(), ["/au_icsm_National_84_02_07_01.tif (range)"]);

    terrio::crs::set_network_enabled(None);
    terrio::crs::reset_grid_cache();
    terrio::config::pop_scope();
}
