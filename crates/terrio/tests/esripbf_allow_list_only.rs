// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! A process-wide allow-list of ESRIPBF alone: one test in a binary of its own, because a list once
//! set cannot be lifted (see `allow_list.rs`). `.pbf` names both drivers, so what a file holds
//! decides.

use terrio::formats::{Kind, VectorDriver, set_allowed_drivers};

#[test]
fn an_allow_list_of_esripbf_opens_it_and_not_openstreetmap() {
    set_allowed_drivers(Some(&["ESRIPBF"]), Some(Kind::Vector)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let options = terrio::VectorOpenOptions::default();
    // A query result holding one field name and no features.
    let result = [&[(1u8 << 3) | 2, 1][..], b"A"].concat();
    let query = [&[(1u8 << 3) | 2, result.len() as u8][..], &result].concat();
    let page = [&[(2u8 << 3) | 2, query.len() as u8][..], &query].concat();
    let path = dir.path().join("a.pbf");
    std::fs::write(&path, &page).unwrap();
    assert_eq!(terrio::open_vector(path.to_str().unwrap(), &options).unwrap().driver, VectorDriver::ESRIPBF);

    let osm = b"\x00\x00\x00\x0d\x0a\x09OSMHeader\x18\x00";
    for name in ["b.pbf", "b.osm.pbf"] {
        let path = dir.path().join(name);
        std::fs::write(&path, osm).unwrap();
        let e = terrio::open_vector(path.to_str().unwrap(), &options).unwrap_err().to_string();
        assert!(e.contains("not allowed"), "{name}: {e}");
    }

    // OpenStreetMap data without its header block, which no content check names: the OpenStreetMap
    // reader would read it, so the list judges that reader.
    let block = [0x0a, 0x00]; // A primitive block with an empty string table.
    let blob = [&[0x0a, block.len() as u8][..], &block].concat(); // Its raw data.
    let header = [&[0x0a, 7][..], b"OSMData", &[0x18, blob.len() as u8]].concat();
    let data = [&(header.len() as u32).to_be_bytes()[..], &header, &blob].concat();
    let path = dir.path().join("c.pbf");
    std::fs::write(&path, &data).unwrap();
    let e = terrio::open_vector(path.to_str().unwrap(), &options).unwrap_err().to_string();
    assert!(e.contains("not allowed"), "{e}");
}
