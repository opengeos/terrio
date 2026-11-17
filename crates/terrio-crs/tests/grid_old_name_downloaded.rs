// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! A grid named as before PROJ 7 with another format's extension (`egm96_15.gtx`) is fetched by its
//! current name (`us_nga_egm96_15.tif`) and read as what it is, a GeoTIFF, also when it is
//! downloaded whole because it cannot be read in place. Its own test binary: the grid transport,
//! network setting and cache directory are process-wide.
#![cfg(all(feature = "grid-transport", not(target_family = "wasm")))]

use std::io::Cursor;
use std::path::Path;
use std::sync::{Arc, Mutex};
use terrio_crs::{
    SpatialRef, Transformer,
    network::{self, GridStream, GridTransport},
};

/// A geoid grid of 2 × 2 nodes, from 110°W to 90°W and 30°N to 50°N, read as pixel-is-point,
/// of 0.5 m everywhere.
fn geoid_grid() -> Vec<u8> {
    use tiff::{
        encoder::{TiffEncoder, colortype::Gray32Float},
        tags::Tag,
    };
    let mut bytes = Cursor::new(Vec::new());
    {
        let mut encoder = TiffEncoder::new(&mut bytes).unwrap();
        let mut image = encoder.new_image::<Gray32Float>(2, 2).unwrap();
        image.encoder().write_tag(Tag::ModelPixelScaleTag, &[20.0_f64, 20.0, 0.0][..]).unwrap();
        image.encoder().write_tag(Tag::ModelTiepointTag, &[0.0_f64, 0.0, 0.0, -110.0, 50.0, 0.0][..]).unwrap();
        image.write_data(&[0.5_f32; 4]).unwrap();
    }
    bytes.into_inner()
}

/// A CDN that cannot be read in place, and gives `us_nga_egm96_15.tif` whole; records the URLs
/// downloaded.
struct WholeOnly(Arc<Mutex<Vec<String>>>);
impl GridTransport for WholeOnly {
    fn open(&self, _: &str) -> std::io::Result<Box<dyn GridStream>> {
        Err(std::io::Error::other("no range requests"))
    }
    fn download(&self, url: &str, path: &Path) -> std::io::Result<()> {
        self.0.lock().unwrap().push(url.to_string());
        if !url.ends_with("/us_nga_egm96_15.tif") {
            return Err(std::io::ErrorKind::NotFound.into());
        }
        std::fs::write(path, geoid_grid())
    }
}

#[test]
fn a_gtx_name_downloaded_as_its_cdn_tif_is_read_as_a_geotiff() {
    let cache = tempfile::tempdir().unwrap();
    // SAFETY: the only test of this binary, before it starts any thread of its own.
    unsafe { std::env::set_var("TERRIO_GRID_CACHE", cache.path()) };
    terrio_crs::set_grid_search_paths(Some(Vec::new()));
    terrio_crs::set_network_enabled(Some(true));
    let urls = Arc::new(Mutex::new(Vec::new()));
    network::set_grid_transport(Arc::new(WholeOnly(urls.clone())));
    let t = Transformer::new(
        &SpatialRef::parse("+proj=longlat +datum=WGS84 +geoidgrids=egm96_15.gtx +vunits=m").unwrap(),
        &SpatialRef::from_epsg(4979).unwrap(),
    )
    .unwrap();
    let (lon, lat, h) = t.transform_3d(-100.0, 40.0, 100.0).unwrap();
    assert!((lon + 100.0).abs() < 1e-12 && (lat - 40.0).abs() < 1e-12 && (h - 100.5).abs() < 1e-9, "({lon}, {lat}, {h})");
    let urls = urls.lock().unwrap();
    assert!(!urls.is_empty() && urls.iter().all(|u| u.ends_with("/us_nga_egm96_15.tif")), "{urls:?}");
}
