// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! A transformation between vertical CRSs whose grid is found, but whose grid in another datum's
//! coordinates needs a horizontal transformation with a missing grid: the missing grid is listed,
//! as PROJ 9.9.0 lists both grids of "Inverse of NZVD2016 height to Auckland 1946 height (2) using
//! NZGD49 to NZGD2000 (3)" for EPSG:4272+5759 to EPSG:4272+7839 at (174.76, -36.85). Its own test
//! binary: the registered grid is process-wide.

use std::io::Cursor;
use terrio_crs::{SpatialRef, Transformer};

/// A geoid-style height grid of 2 × 2 nodes, from 174°E to 176°E and 38°S to 36°S, read as
/// pixel-is-point, of 0.5 m everywhere.
fn height_grid() -> Vec<u8> {
    use tiff::{
        encoder::{TiffEncoder, colortype::Gray32Float},
        tags::Tag,
    };
    let mut bytes = Cursor::new(Vec::new());
    {
        let mut encoder = TiffEncoder::new(&mut bytes).unwrap();
        let mut image = encoder.new_image::<Gray32Float>(2, 2).unwrap();
        image.encoder().write_tag(Tag::ModelPixelScaleTag, &[2.0_f64, 2.0, 0.0][..]).unwrap();
        image.encoder().write_tag(Tag::ModelTiepointTag, &[0.0_f64, 0.0, 0.0, 174.0, -36.0, 0.0][..]).unwrap();
        image.write_data(&[0.5_f32; 4]).unwrap();
    }
    bytes.into_inner()
}

#[test]
fn a_missing_grid_of_the_interpolation_route_is_listed() {
    terrio_crs::set_grid_search_paths(Some(Vec::new()));
    terrio_crs::set_network_enabled(Some(false));
    terrio_crs::register_grid("nz_linz_auckht1946-nzvd2016.tif", height_grid());
    let t = Transformer::new(&SpatialRef::parse("EPSG:4272+5759").unwrap(), &SpatialRef::parse("EPSG:4272+7839").unwrap()).unwrap();
    let uses = t.best_operation_grids_at(174.76, -36.85, 0.0).unwrap();
    let got: Vec<(&str, bool)> = uses.iter().map(|g| (g.name.as_str(), g.available)).collect();
    assert_eq!(got, [("nz_linz_auckht1946-nzvd2016.tif", true), ("nz_linz_nzgd2kgrid0005.tif", false)]);
}
