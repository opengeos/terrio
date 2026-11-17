// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! With the network on, a grid on the PROJ CDN counts as available until it is fetched. When the
//! fetch fails while the best operation is tried, the transformation stops there, as PROJ's
//! `proj_trans` stops on a network error, and `Transformer::best_operation_grids_at` lists that
//! operation's grids, not a later operation's. Its own test binary: the grid transport and
//! network setting are process-wide.
#![cfg(all(feature = "grid-transport", not(target_family = "wasm")))]

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use terrio_crs::{
    SpatialRef, Transformer,
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
fn a_grid_whose_first_download_fails_is_listed() {
    terrio_crs::set_grid_search_paths(Some(Vec::new()));
    terrio_crs::set_network_enabled(Some(true));
    let attempts = Arc::new(AtomicUsize::new(0));
    network::set_grid_transport(Arc::new(Unreachable(attempts.clone())));
    for (source, x, y, expected) in [
        // NAD27 to WGS 84 (48), a concatenation through NAD83.
        ("EPSG:4267", -100.0, 40.0, &["us_noaa_conus.tif", "us_noaa_kshpgn.tif"][..]),
        // OSGB36 to WGS 84 (9), a single transformation.
        ("EPSG:4277", -2.0, 52.6, &["uk_os_OSTN15_NTv2_OSGBtoETRS.tif"]),
    ] {
        let t = Transformer::new(&SpatialRef::parse(source).unwrap(), &SpatialRef::from_epsg(4326).unwrap()).unwrap();
        let before = attempts.load(Ordering::SeqCst);
        let uses = t.best_operation_grids_at(x, y, 0.0).unwrap();
        let names: Vec<&str> = uses.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, expected, "{source}");
        // Missing once its fetch has failed (a later grid of the operation was not tried).
        assert!(!uses[0].available, "{uses:?}");
        // The fetch was tried, by this query.
        assert!(attempts.load(Ordering::SeqCst) > before, "{source}");
    }
    // NGVD29 height (m) to NAVD88 height (2), VERTCON central, whose fetch fails when it runs:
    // listed, not the next transformation's grids or none.
    let t = Transformer::new(&SpatialRef::parse("EPSG:4267+7968").unwrap(), &SpatialRef::parse("EPSG:4269+5703").unwrap()).unwrap();
    let uses = t.best_operation_grids_at(-100.0, 40.0, 0.0).unwrap();
    let names: Vec<&str> = uses.iter().map(|g| g.name.as_str()).collect();
    assert_eq!(names, ["us_noaa_nadcon5_nad27_nad83_1986_conus.tif", "us_noaa_vertconc.tif"]);
}
