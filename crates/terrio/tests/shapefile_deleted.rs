// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Deleted DBF slots preserve physical FIDs; paging counts live features.
//! The GDAL comparison is registered as `shapefile-deleted` in scripts/conformance.py.
use terrio::{Feature, FieldDef, FieldType, FieldValue, Geometry, Layer, VectorOpenOptions};

/// The deletion sets each test reads: some slots (first and last included), all, none.
const DELETIONS: [&[usize]; 5] = [&[0, 2, 3, 5], &[0, 1, 2, 3, 4, 5], &[], &[1], &[0, 1, 2, 3, 4]];

/// Six points written as a shapefile in `dir`, the path of its `.shp` and its DBF's bytes.
fn six_points(dir: &std::path::Path) -> (Layer, String, Vec<u8>) {
    let mut layer = Layer::new("deleted");
    layer.add_field(FieldDef::new("id", FieldType::Integer));
    for i in 0..6 {
        layer.push(Feature {
            fid: i,
            geometry: (i != 4).then(|| Geometry::point(i as f64, 0.0)),
            attributes: vec![FieldValue::Integer(9007199254740993 + i as i64)],
        });
    }
    let path = dir.join("deleted.shp").to_str().unwrap().to_string();
    terrio::write_vector(&[layer.clone()], &path, &Default::default()).unwrap();
    let dbf = std::fs::read(std::path::Path::new(&path).with_extension("dbf")).unwrap();
    (layer, path, dbf)
}

/// Rewrites the shapefile's DBF from `original` with the given slots flagged deleted.
fn delete(path: &str, original: &[u8], deleted: &[usize]) {
    let header = u16::from_le_bytes([original[8], original[9]]) as usize;
    let width = u16::from_le_bytes([original[10], original[11]]) as usize;
    let mut dbf = original.to_vec();
    for &i in deleted {
        dbf[header + i * width] = b'*';
    }
    std::fs::write(std::path::Path::new(path).with_extension("dbf"), dbf).unwrap();
}

/// Checks that `actual` holds the features of `layer` that are not deleted, as written.
fn assert_live(actual: &[Feature], layer: &Layer, deleted: &[usize]) {
    let expected: Vec<_> = layer.features.iter().filter(|f| !deleted.contains(&(f.fid as usize))).collect();
    assert_eq!(actual.len(), expected.len(), "deleted {deleted:?}");
    for (got, want) in actual.iter().zip(expected) {
        assert_eq!(got.fid, want.fid);
        assert_eq!(got.geometry, want.geometry);
        assert_eq!(got.attributes, want.attributes);
    }
}

/// Whole-file reads retain every live physical ID, geometry and exact integer.
#[test]
fn deleted_slots_keep_public_feature_ids() {
    let dir = tempfile::tempdir().unwrap();
    let (layer, path, original) = six_points(dir.path());
    for deleted in DELETIONS {
        delete(&path, &original, deleted);
        let data = terrio::open_vector(&path, &Default::default()).unwrap();
        assert_live(&data.layers[0].features, &layer, deleted);
    }
}

/// Public paging agrees with slicing a whole read, with or without the SHX index.
#[test]
fn ranges_skip_deleted_slots() {
    let dir = tempfile::tempdir().unwrap();
    let (layer, path, original) = six_points(dir.path());
    for indexed in [true, false] {
        if !indexed {
            std::fs::remove_file(std::path::Path::new(&path).with_extension("shx")).unwrap();
        }
        for deleted in DELETIONS {
            delete(&path, &original, deleted);
            let live: Vec<_> = layer.features.iter().filter(|f| !deleted.contains(&(f.fid as usize))).collect();
            for (skip, max) in
                [(0, Some(0)), (0, Some(2)), (1, Some(4)), (2, Some(2)), (5, Some(1)), (6, None), (usize::MAX, Some(2)), (1, None)]
            {
                let options = VectorOpenOptions { skip_features: skip, max_features: max, ..Default::default() };
                let data = terrio::open_vector(&path, &options).unwrap();
                let actual = &data.layers[0].features;
                let expected: Vec<_> = live.iter().skip(skip).take(max.unwrap_or(usize::MAX)).collect();
                assert_eq!(actual.len(), expected.len(), "indexed {indexed}, deleted {deleted:?}, skip {skip}, max {max:?}");
                for (got, want) in actual.iter().zip(expected) {
                    assert_eq!(got.fid, want.fid);
                    assert_eq!(got.geometry, want.geometry);
                    assert_eq!(got.attributes, want.attributes);
                }
            }
            // A paging client may stop on a short page without losing later live features.
            let mut ids = Vec::new();
            for skip in (0..=6).step_by(2) {
                let options = VectorOpenOptions { skip_features: skip, max_features: Some(2), ..Default::default() };
                let page = terrio::open_vector(&path, &options).unwrap();
                ids.extend(page.layers[0].features.iter().map(|f| f.fid));
                if page.layers[0].features.len() < 2 {
                    break;
                }
            }
            assert_eq!(ids, live.iter().map(|f| f.fid).collect::<Vec<_>>());
        }
    }
}

/// Missing DBF slots are live shapes with null attributes, rather than deleted rows.
#[test]
fn ranges_preserve_shapes_without_dbf_records() {
    for absent in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (_, path, mut dbf) = six_points(dir.path());
        let dbf_path = std::path::Path::new(&path).with_extension("dbf");
        if absent {
            std::fs::remove_file(dbf_path).unwrap();
        } else {
            let header = u16::from_le_bytes([dbf[8], dbf[9]]) as usize;
            let width = u16::from_le_bytes([dbf[10], dbf[11]]) as usize;
            dbf[4..8].copy_from_slice(&1u32.to_le_bytes());
            dbf.truncate(header + width);
            std::fs::write(dbf_path, dbf).unwrap();
        }
        let options = VectorOpenOptions { skip_features: 1, max_features: Some(2), ..Default::default() };
        let data = terrio::open_vector(&path, &options).unwrap();
        let features = &data.layers[0].features;
        assert_eq!(features.iter().map(|f| f.fid).collect::<Vec<_>>(), [1, 2]);
        assert!(features.iter().all(|f| f.attributes.iter().all(|v| *v == FieldValue::Null)));
    }
}
