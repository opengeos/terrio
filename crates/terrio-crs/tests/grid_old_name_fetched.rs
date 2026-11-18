// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! With the network on, a grid a PROJ string names as before PROJ 7 (`+nadgrids=conus`) counts as
//! available and is fetched from the CDN by its current name (`us_noaa_conus.tif`), as PROJ 9.9.0
//! fetches `https://cdn.proj.org/us_noaa_conus.tif` for it (`PROJ_NETWORK=ON`, no grid installed).
//! Its own test binary: the grid transport and network setting are process-wide.
#![cfg(all(feature = "grid-transport", not(target_family = "wasm")))]

use std::io::Cursor;
use std::sync::{Arc, Mutex};
use terrio_crs::{
    SpatialRef, Transformer,
    network::{self, GridStream, GridTransport},
};

/// A horizontal shift grid of 2 × 2 nodes, from 130°W to 90°W and 25°N to 55°N, read as
/// pixel-is-point, shifting latitudes by 1″ and longitudes by 2″ everywhere.
fn shift_grid() -> Vec<u8> {
    use tiff::{
        encoder::{TiffEncoder, colortype::RGB32Float},
        tags::Tag,
    };
    let mut bytes = Cursor::new(Vec::new());
    {
        let mut encoder = TiffEncoder::new(&mut bytes).unwrap();
        let mut image = encoder.new_image::<RGB32Float>(2, 2).unwrap();
        image.encoder().write_tag(Tag::ModelPixelScaleTag, &[40.0_f64, 30.0, 0.0][..]).unwrap();
        image.encoder().write_tag(Tag::ModelTiepointTag, &[0.0_f64, 0.0, 0.0, -130.0, 55.0, 0.0][..]).unwrap();
        image.write_data(&[1.0_f32, 2.0, 0.0].repeat(4)).unwrap();
    }
    bytes.into_inner()
}

/// Serves the grid as `us_noaa_conus.tif`, and records the URLs asked for.
struct Cdn(Arc<Mutex<Vec<String>>>);
impl GridTransport for Cdn {
    fn open(&self, url: &str) -> std::io::Result<Box<dyn GridStream>> {
        self.0.lock().unwrap().push(url.to_string());
        if url.ends_with("/us_noaa_conus.tif") { Ok(Box::new(Cursor::new(shift_grid()))) } else { Err(std::io::ErrorKind::NotFound.into()) }
    }
}

#[test]
fn a_grid_named_as_before_proj_7_is_fetched_by_its_cdn_name() {
    terrio_crs::set_grid_search_paths(Some(Vec::new()));
    terrio_crs::set_network_enabled(Some(true));
    let urls = Arc::new(Mutex::new(Vec::new()));
    network::set_grid_transport(Arc::new(Cdn(urls.clone())));
    let t = Transformer::new(
        &SpatialRef::from_proj4("+proj=longlat +ellps=clrk66 +nadgrids=conus").unwrap(),
        &SpatialRef::from_epsg(4326).unwrap(),
    )
    .unwrap();
    let uses = t.best_operation_grids_at(-100.0, 40.0, 0.0).unwrap();
    assert_eq!((uses[0].name.as_str(), uses[0].available), ("us_noaa_conus.tif", true));
    let (lon, lat) = t.transform(-100.0, 40.0).unwrap();
    assert!((lon - (-100.0 + 2.0 / 3600.0)).abs() < 1e-9 && (lat - (40.0 + 1.0 / 3600.0)).abs() < 1e-9, "({lon}, {lat})");
    let urls = urls.lock().unwrap();
    assert!(!urls.is_empty() && urls.iter().all(|u| u.ends_with("/us_noaa_conus.tif")), "{urls:?}");
}
