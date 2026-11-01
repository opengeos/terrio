// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! The protocol buffer of feature-service queries under a process-wide allow-list of OpenStreetMap
//! alone: one test in a binary of its own, because a list once set cannot be lifted (see
//! `allow_list.rs`).

use terrio::formats::{Kind, set_allowed_drivers};

#[test]
fn an_allow_list_of_osm_refuses_esripbf_under_its_extension() {
    set_allowed_drivers(Some(&["OSM"]), Some(Kind::Vector)).unwrap();
    // A query result holding one field name, enough to show the format.
    let result = [&[(1u8 << 3) | 2, 1][..], b"A"].concat();
    let query = [&[(1u8 << 3) | 2, result.len() as u8][..], &result].concat();
    let page = [&[(2u8 << 3) | 2, query.len() as u8][..], &query].concat();
    let dir = tempfile::tempdir().unwrap();
    let options = terrio::VectorOpenOptions::default();
    let path = dir.path().join("a.pbf");
    std::fs::write(&path, &page).unwrap();
    let refused = |r: terrio::Result<terrio::VectorDataset>| r.is_err_and(|e| e.to_string().contains("not allowed"));
    assert!(refused(terrio::open_vector(path.to_str().unwrap(), &options)));
    assert!(refused(terrio::memory::open_vector_bytes(page, "a.pbf", &options)));
}
