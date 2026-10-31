//! Zstandard GeoParquet read without the zstd C library, from the files of
//! `tests/data/parquet_zstd` (`scripts/make_parquet_zstd_fixtures.py`, written with pyarrow): the
//! same table as the Snappy file, whole, with a spatial filter (which skips rows and pages) and by
//! row group. GDAL 3.13.3 reads 500 features from each, and 30 inside the filter.
#![cfg(feature = "parquet")]

use std::path::PathBuf;

use terrio::{Layer, VectorOpenOptions};

const FILES: [&str; 3] = ["zstd_v1.parquet", "zstd_v2.parquet", "zstd_mixed.parquet"];
const FILTER: (f64, f64, f64, f64) = (-5.5, -2.5, 5.5, 2.5);

fn read(file: &str, spatial_filter: Option<(f64, f64, f64, f64)>) -> Layer {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/parquet_zstd").join(file);
    let options = VectorOpenOptions { spatial_filter, ..Default::default() };
    terrio::open_vector(path.to_str().unwrap(), &options).unwrap().layers.remove(0)
}

fn assert_same(file: &str, got: &Layer, want: &Layer) {
    assert_eq!(got.len(), want.len(), "{file}");
    assert_eq!(got.geom_type, want.geom_type, "{file}");
    let names = |l: &Layer| l.schema.fields().iter().map(|f| f.name.clone()).collect::<Vec<_>>();
    assert_eq!(names(got), names(want), "{file}");
    for (a, b) in got.features.iter().zip(&want.features) {
        assert_eq!(a.fid, b.fid, "{file}");
        assert_eq!(a.geometry, b.geometry, "{file}");
        assert_eq!(a.attributes, b.attributes, "{file}");
    }
}

#[test]
fn zstd_pages_read_like_snappy() {
    let want = read("snappy.parquet", None);
    assert_eq!(want.len(), 500);
    for file in FILES {
        assert_same(file, &read(file, None), &want);
    }
}

#[test]
fn zstd_pages_read_with_a_spatial_filter() {
    let want = read("snappy.parquet", Some(FILTER));
    assert_eq!(want.len(), 30);
    for file in FILES {
        assert_same(file, &read(file, Some(FILTER)), &want);
    }
}
