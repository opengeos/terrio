//! An earliest-format HDF5 file whose version 1 compound datatype has a member name off the 8-byte
//! grid (a float member, then a fixed-length string), as HEC-RAS writes its attribute tables. The
//! file must open, and its two-dimensional dataset read as a raster. hdf5-reader 0.9.1 failed the
//! whole file ("unsupported datatype class 13"); fixed in roteiro-gis/netcdf-rust#95.
//!
//! The fixture was written by h5py with `libver="earliest"`:
//! a group `Geometry/2D Flow Areas` holding a compound dataset `Attributes`
//! (`x` float32, `name` S16, `count` int32), and a float32 dataset `grid` of 4 x 5 values 0..19.

use std::path::PathBuf;

fn fixture() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/hdf5_v1_compound_table.h5").to_string_lossy().into_owned()
}

#[test]
fn opens_a_file_with_a_v1_compound_table() {
    let ds = terrio::open_raster(&fixture()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!((ds.raster.cols, ds.raster.rows), (5, 4));
    let values = ds.raster.data.to_f64_vec();
    assert_eq!(values.first().copied(), Some(0.0));
    assert_eq!(values.last().copied(), Some(19.0));
}
