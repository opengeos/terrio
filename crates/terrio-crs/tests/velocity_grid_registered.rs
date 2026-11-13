//! A point motion with its velocity grid there: a point inside the grid moves, and a point outside
//! it fails as a point outside the domain (`Error::Crs`), not as a missing grid. Its own test
//! binary: the registered grid is process-wide, and other tests need the grid missing. Grids and
//! the network are off, so that only the registered grid can be found.

use std::io::Cursor;
use terrio_crs::{Error, SpatialRef, TransformOptions, Transformer};

/// A velocity grid of 2 × 2 nodes 10° apart, read as pixel-is-point (no GeoKey directory), so
/// from 80°W to 70°W and 40°N to 50°N, with an east, north
/// and up velocity of 10, 20 and 0 mm a year everywhere (bands in that order, as the Canada grids
/// have them).
fn velocity_grid() -> Vec<u8> {
    use tiff::{
        encoder::{TiffEncoder, colortype::RGB32Float},
        tags::Tag,
    };
    let mut bytes = Cursor::new(Vec::new());
    {
        let mut encoder = TiffEncoder::new(&mut bytes).unwrap();
        let mut image = encoder.new_image::<RGB32Float>(2, 2).unwrap();
        image.encoder().write_tag(Tag::ModelPixelScaleTag, &[10.0_f64, 10.0, 0.0][..]).unwrap();
        image.encoder().write_tag(Tag::ModelTiepointTag, &[0.0_f64, 0.0, 0.0, -80.0, 50.0, 0.0][..]).unwrap();
        image.write_data(&[10.0_f32, 20.0, 0.0].repeat(4)).unwrap();
    }
    bytes.into_inner()
}

#[test]
fn a_point_outside_a_present_velocity_grid_fails_as_outside_the_domain() {
    terrio_crs::set_grid_search_paths(Some(Vec::new()));
    terrio_crs::set_network_enabled(Some(false));
    // NAD83(CSRS)v6 between epochs 2010 and 2020: the Canada velocity grid v6.
    terrio_crs::register_grid("ca_nrc_NAD83v6VG.tif", velocity_grid());
    let csrs = SpatialRef::from_epsg(8252).unwrap();
    let opts = TransformOptions { source_coordinate_epoch: Some(2010.0), target_coordinate_epoch: Some(2020.0), ..Default::default() };
    let t = Transformer::with_options(&csrs, &csrs, &opts).unwrap();
    // Inside: moved 10 years at 10 mm/yr east and 20 mm/yr north, so by 0.1 m and 0.2 m.
    let (lon, lat) = t.transform(-75.0, 45.0).unwrap();
    let (east, north) = ((lon + 75.0).to_radians() * 6_378_137.0 * 45f64.to_radians().cos(), (lat - 45.0).to_radians() * 6_378_137.0);
    assert!((east - 0.1).abs() < 2e-3 && (north - 0.2).abs() < 2e-3, "moved by ({east}, {north}) m");
    // Outside: a point outside the grid, not a missing grid.
    assert!(matches!(t.transform(-100.0, 40.0), Err(Error::Crs(_))));
    let mut xy = vec![(-75.0, 45.0), (-100.0, 40.0)];
    assert_eq!(t.transform_slice_or_nan(&mut xy).unwrap(), 1);
    assert!(xy[1].0.is_nan());
}
