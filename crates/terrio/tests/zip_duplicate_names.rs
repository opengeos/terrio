// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! Duplicate member names are ambiguous for byte, whole-archive and member opens.

const DUPLICATES: &[u8] = include_bytes!("data/zip/duplicate_members.zip");

fn ambiguous(result: terrio::Result<()>) {
    let error = result.unwrap_err();
    assert!(matches!(&error, terrio::Error::InvalidData(message) if message.contains("several entries with the same name")), "{error:?}");
}

#[test]
fn duplicate_member_names_are_refused_from_bytes() {
    ambiguous(terrio::memory::open_bytes(DUPLICATES.to_vec(), "vector.zip").map(|_| ()));
    ambiguous(terrio::memory::open_vector_bytes(DUPLICATES.to_vec(), "vector.zip", &Default::default()).map(|_| ()));
}

#[cfg(fs)]
#[test]
fn duplicate_member_names_are_refused_from_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vector.zip");
    std::fs::write(&path, DUPLICATES).unwrap();
    let path = path.to_str().unwrap();
    for source in [path.to_string(), format!("/vsizip/{path}/data.geojson")] {
        ambiguous(terrio::open(&source).map(|_| ()));
        ambiguous(terrio::open_vector(&source, &Default::default()).map(|_| ()));
        ambiguous(terrio::list_layers(&source).map(|_| ()));
    }
}
