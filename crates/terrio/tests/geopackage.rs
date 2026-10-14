//! GeoPackage spatial filters through the R-tree, and updates of existing files, on real-world
//! files, with expected values from GDAL 3.12 (`ogr2ogr -spat`, `ogr2ogr -append`; see
//! `scripts/compare_gpkg_with_gdal.py`).
//!
//! The files live outside the repository: set `TERRIO_TEST_DATA` to their directory (see
//! `scripts/fetch_test_data.sh`). Without it, these tests pass without checking anything.

use std::path::PathBuf;
use terrio::VectorOpenOptions;
use terrio::ops::vector_filter::{VectorTranslateOptions, WriteMode, translate, write_vector_with_mode};

fn data(name: &str) -> Option<String> {
    let dir = PathBuf::from(std::env::var_os("TERRIO_TEST_DATA")?);
    let p = dir.join(name);
    assert!(p.exists(), "{} is missing from TERRIO_TEST_DATA", p.display());
    Some(p.to_string_lossy().into_owned())
}

fn open(path: &str, bbox: Option<(f64, f64, f64, f64)>) -> terrio::Layer {
    let o = VectorOpenOptions { spatial_filter: bbox, ..Default::default() };
    terrio::open_vector(path, &o).unwrap().layers.remove(0)
}

fn bbox_hits(layer: &terrio::Layer, (x0, y0, x1, y1): (f64, f64, f64, f64)) -> Vec<u64> {
    layer
        .features
        .iter()
        .filter(|f| {
            f.geometry.as_ref().and_then(|g| g.bbox()).is_some_and(|b| b.min_x <= x1 && b.max_x >= x0 && b.min_y <= y1 && b.max_y >= y0)
        })
        .map(|f| f.fid)
        .collect()
}

#[test]
fn rtree_filters_match_a_scan_and_gdal() {
    // (file, rectangle, features ogr2ogr -spat keeps)
    for (name, rect, expected) in [("UScounties.gpkg", (-90.0, 35.0, -85.0, 40.0), 253), ("countries.gpkg", (-10.0, 35.0, 30.0, 60.0), 42)]
    {
        let Some(path) = data(name) else { return };
        let all = open(&path, None);
        let filtered = open(&path, Some(rect));
        // The R-tree finds the features a scan finds, with their ids and values.
        assert_eq!(filtered.features.iter().map(|f| f.fid).collect::<Vec<_>>(), bbox_hits(&all, rect), "{name}");
        for f in &filtered.features {
            let same = all.features.iter().find(|g| g.fid == f.fid).unwrap();
            assert_eq!(f.attributes, same.attributes);
            assert_eq!(f.geometry, same.geometry);
        }
        // And the exact filter keeps what GDAL keeps.
        let o = VectorTranslateOptions { spatial_filter: Some(rect), ..Default::default() };
        assert_eq!(translate(&[filtered], &o).unwrap()[0].len(), expected, "{name}");
    }
}

#[test]
fn appends_to_a_gdal_file_as_ogr2ogr() {
    let Some(path) = data("countries.gpkg") else { return };
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("c.gpkg");
    std::fs::copy(&path, &out).unwrap();
    let out = out.to_str().unwrap();
    let layer = open(&path, None);
    write_vector_with_mode(std::slice::from_ref(&layer), out, &Default::default(), WriteMode::Append).unwrap();
    let back = open(out, None);
    // ogr2ogr -append: 352 features, new ids from 177.
    assert_eq!(back.len(), 352);
    assert_eq!(back.features[176].fid, 177);
    assert_eq!(back.features[176].attributes, layer.features[0].attributes);
    assert_eq!(back.features[351].geometry, layer.features[175].geometry);
    // The rebuilt R-tree finds both copies.
    let rect = (-10.0, 35.0, 30.0, 60.0);
    assert_eq!(bbox_hits(&open(out, Some(rect)), rect).len(), 2 * bbox_hits(&layer, rect).len());
    // Overwriting gives the layer back.
    write_vector_with_mode(std::slice::from_ref(&layer), out, &Default::default(), WriteMode::Overwrite).unwrap();
    assert_eq!(open(out, None).len(), 176);
}
