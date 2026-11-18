// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! A grid registered under its name on the PROJ CDN is found by a PROJ string naming it as before
//! PROJ 7 (`+nadgrids=conus` finds `us_noaa_conus.tif`), as PROJ finds it through its database,
//! and is then listed as available. The way an application that downloads grids itself (in
//! WebAssembly too) supplies them. Its own test binary: the registered grid is process-wide.
//! Grids and the network are off, so that only the registered grid can be found.

use std::io::Cursor;
use terrio_crs::{SpatialRef, Transformer};

/// A horizontal shift grid of 2 × 2 nodes, from 130°W to 90°W and 25°N to 55°N, read as
/// pixel-is-point, shifting latitudes by 1″ and longitudes by 2″ everywhere (bands in that order,
/// as PROJ reads a grid without band descriptions).
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

#[test]
fn a_grid_named_as_before_proj_7_is_found_under_its_cdn_name() {
    terrio_crs::set_grid_search_paths(Some(Vec::new()));
    terrio_crs::set_network_enabled(Some(false));
    let (nad27, wgs84) =
        (SpatialRef::from_proj4("+proj=longlat +ellps=clrk66 +nadgrids=conus").unwrap(), SpatialRef::from_epsg(4326).unwrap());
    let before = Transformer::new(&nad27, &wgs84).unwrap();
    assert!(!before.best_operation_grids_at(-100.0, 40.0, 0.0).unwrap()[0].available);
    // Used before the grid is there: the lookup of `conus` finds nothing, and is forgotten when
    // the grid is registered under its other name.
    let _ = before.transform(-100.0, 40.0);
    terrio_crs::register_grid("us_noaa_conus.tif", shift_grid());
    let after = Transformer::new(&nad27, &wgs84).unwrap();
    let uses = after.best_operation_grids_at(-100.0, 40.0, 0.0).unwrap();
    assert_eq!((uses[0].name.as_str(), uses[0].available), ("us_noaa_conus.tif", true));
    let (lon, lat) = after.transform(-100.0, 40.0).unwrap();
    assert!((lon - (-100.0 + 2.0 / 3600.0)).abs() < 1e-9 && (lat - (40.0 + 1.0 / 3600.0)).abs() < 1e-9, "({lon}, {lat})");
}
