// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! A CRS used before a grid it can use is registered uses the grid after, though its definition is
//! kept between uses (opengeos/terrio#1058, step 0b). Its own test binary: the registered grid is
//! process-wide. Grids and the network are off, so that only the registered grid can be found.

use std::io::Cursor;
use terrio_crs::{SpatialRef, Transformer};

/// NOAA's NAD27 to NAD83 grid for the conterminous US, by name: a horizontal shift grid of 2 × 2
/// nodes, from 130°W to 90°W and 25°N to 55°N, read as pixel-is-point, shifting latitudes by 1″
/// and longitudes by 2″ everywhere (bands in that order, as PROJ reads a grid without band
/// descriptions).
fn conus_grid() -> Vec<u8> {
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
fn a_grid_registered_after_a_crs_was_used_is_used() {
    terrio_crs::set_grid_search_paths(Some(Vec::new()));
    terrio_crs::set_network_enabled(Some(false));
    let (nad27, wgs84) = (SpatialRef::from_epsg(4267).unwrap(), SpatialRef::from_epsg(4326).unwrap());
    // Without the grid: a Helmert transformation.
    let before = Transformer::new(&nad27, &wgs84).unwrap();
    assert_eq!(before.operation_at(-100.0, 40.0, 0.0).unwrap().unwrap().name, "NAD27 to WGS 84 (6)");
    terrio_crs::register_grid("us_noaa_conus.tif", conus_grid());
    // With it: the transformation through the grid, which the definition of NAD27 built before
    // did not hold.
    let after = Transformer::new(&nad27, &wgs84).unwrap();
    let used = after.operation_at(-100.0, 40.0, 0.0).unwrap().unwrap();
    assert_eq!(used.name, "NAD27 to WGS 84 (79)");
    assert_eq!(used.grids, ["us_noaa_conus.tif"]);
    let (lon, lat) = after.transform(-100.0, 40.0).unwrap();
    assert!((lon - (-100.0 + 2.0 / 3600.0)).abs() < 1e-9 && (lat - (40.0 + 1.0 / 3600.0)).abs() < 1e-9, "({lon}, {lat})");
}
