// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! The protocol buffer of feature-service queries, recognized by its content.

use terrio::formats::VectorDriver;
use wbvector::{FieldValue, Geometry};

fn message(field: u8, bytes: &[u8]) -> Vec<u8> {
    assert!(bytes.len() < 128, "a one-byte length");
    [&[(field << 3) | 2, bytes.len() as u8][..], bytes].concat()
}

/// One point feature (OBJECTID 7, NAME "Tunja") with no transform, at (1, -2): Y goes down from 0.
fn page() -> Vec<u8> {
    let field = |name: &str, kind: u8| message(13, &[message(1, name.as_bytes()), vec![2 << 3, kind]].concat());
    let oid = field("OBJECTID", 6);
    let name = field("NAME", 4);
    let attributes = [message(1, &[5 << 3, 7]), message(1, &message(1, b"Tunja"))].concat();
    let geometry = message(2, &message(3, &[2, 4]));
    let feature = message(15, &[attributes, geometry].concat());
    let result = [oid, name, feature].concat();
    message(2, &message(1, &result))
}

fn check(ds: &terrio::VectorDataset) {
    assert_eq!(ds.driver, VectorDriver::ESRIPBF);
    let l = &ds.layers[0];
    assert_eq!(l.features.len(), 1);
    assert_eq!(l.features[0].fid, 7);
    assert_eq!(l.features[0].attributes[1], FieldValue::Text("Tunja".into()));
    assert!(matches!(&l.features[0].geometry, Some(Geometry::Point(c)) if (c.x, c.y) == (1.0, -2.0)));
}

#[test]
fn recognized_by_content_from_files_and_memory() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["cities.pbf", "query"] {
        let path = dir.path().join(name);
        std::fs::write(&path, page()).unwrap();
        check(&terrio::open_vector(path.to_str().unwrap(), &Default::default()).unwrap());
        assert!(matches!(terrio::open(path.to_str().unwrap()), Ok(terrio::Dataset::Vector(ds)) if ds.driver == VectorDriver::ESRIPBF));
        check(&terrio::memory::open_vector_bytes(page(), name, &Default::default()).unwrap());
    }
    assert_eq!(terrio::inspection::identify_bytes(&page(), "cities.pbf").unwrap().driver, "ESRIPBF");
    assert_eq!(terrio::formats::sniff(&page()), Some("ESRIPBF"));
}

#[test]
fn pbf_still_names_openstreetmap_and_esripbf_is_not_written() {
    assert_eq!(VectorDriver::for_extension("out.pbf"), Some(VectorDriver::OSM));
    assert!(!VectorDriver::ESRIPBF.info().write);
    assert_eq!(VectorDriver::from_name("esripbf"), Some(VectorDriver::ESRIPBF));
}

#[test]
fn openstreetmap_pbf_is_told_apart() {
    // The start of an OpenStreetMap file: a header length, then a blob header of type `OSMHeader`.
    let osm = b"\x00\x00\x00\x0d\x0a\x09OSMHeader\x18\x00";
    assert_eq!(terrio::formats::sniff(osm), Some("OSM"));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cut.pbf");
    std::fs::write(&path, osm).unwrap();
    let path = path.to_str().unwrap();
    assert_eq!(terrio::inspection::identify(path, false).unwrap().unwrap().driver, "OSM");
    // The OpenStreetMap reader reads it, as empty layers.
    assert_eq!(terrio::open_vector(path, &Default::default()).unwrap().driver, VectorDriver::OSM);
}

#[test]
fn a_cut_file_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cut.pbf");
    let whole = page();
    std::fs::write(&path, &whole[..whole.len() - 3]).unwrap();
    assert!(terrio::open_vector(path.to_str().unwrap(), &Default::default()).is_err());
}

/// A message whose length may need more than one byte.
fn long_message(field: u8, bytes: &[u8]) -> Vec<u8> {
    let mut out = vec![(field << 3) | 2];
    let mut n = bytes.len() as u64;
    while n >= 0x80 {
        out.push((n as u8 & 0x7f) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
    [out, bytes.to_vec()].concat()
}

#[test]
fn fields_times_features_cannot_exceed_what_the_answer_justifies() {
    // 20,000 fields and 1,000,000 empty features: about 2 MB, and 20 billion values.
    let fields: Vec<u8> =
        (0..20_000).flat_map(|i| message(13, &[message(1, format!("F{i}").as_bytes()), vec![2 << 3, 4]].concat())).collect();
    let features = message(15, &[]).repeat(1_000_000);
    let answer = long_message(2, &long_message(1, &[fields, features].concat()));
    // Refused by every way in, and not retried with GDAL in builds with its backend.
    let refused = |r: terrio::Result<terrio::VectorDataset>| {
        r.is_err_and(|e| matches!(&e, terrio::Error::InvalidArgument(m) if m.contains("features of 20000 fields")))
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("q.pbf");
    std::fs::write(&path, &answer).unwrap();
    assert!(refused(terrio::open_vector(path.to_str().unwrap(), &Default::default())));
    assert!(refused(terrio::memory::open_vector_bytes(answer, "q.pbf", &Default::default())));
}
