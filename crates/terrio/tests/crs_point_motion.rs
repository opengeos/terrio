//! Point motion operations, coordinates moved between two epochs of one dynamic CRS by a
//! velocity grid read from the PROJ CDN: `cargo test -p terrio --test crs_point_motion -- --ignored`.
//!
//! The expected values are PROJ 9.8.1's (`projinfo -s EPSG:8254@2010 -t EPSG:8254@2020`).

use terrio::crs::{SpatialRef, TransformOptions, Transformer};

fn moved(code: &str, from: f64, to: f64, p: (f64, f64, f64)) -> (f64, f64, f64) {
    let crs = SpatialRef::parse(code).unwrap();
    let opts = TransformOptions { source_coordinate_epoch: Some(from), target_coordinate_epoch: Some(to) };
    Transformer::with_options(&crs, &crs, &opts).unwrap().transform_3d(p.0, p.1, p.2).unwrap()
}

#[test]
#[ignore = "network"]
fn velocity_grids_as_proj() {
    terrio::crs::set_grid_search_paths(Some(Vec::new()));
    terrio::crs::set_network_enabled(Some(true));
    let close = |a: (f64, f64, f64), b: (f64, f64, f64)| (a.0 - b.0).abs() < 1e-12 && (a.1 - b.1).abs() < 1e-12 && (a.2 - b.2).abs() < 1e-7;
    // Canada velocity grid v7, NAD83(CSRS)v7 3D and 2D, both ways.
    let p = moved("EPSG:8254", 2010.0, 2020.0, (-75.7, 45.4, 100.0));
    assert!(close(p, (-75.69999975642823, 45.399999826914204, 100.02073319535702)), "{p:?}");
    let p = moved("EPSG:8255", 2010.0, 2020.0, (-75.7, 45.4, 100.0));
    assert!(close(p, (-75.69999975642823, 45.399999826914204, 100.02073319535702)), "{p:?}");
    let p = moved("EPSG:8254", 2020.0, 2010.0, (-75.7, 45.4, 100.0));
    assert!(close(p, (-75.70000024357178, 45.4000001730858, 99.97926680464298)), "{p:?}");
    // Projected CRSs are not moved, as PROJ does not move them.
    let p = moved("EPSG:22718", 2010.0, 2020.0, (450000.0, 5030000.0, 100.0));
    assert_eq!(p, (450000.0, 5030000.0, 100.0));
}
