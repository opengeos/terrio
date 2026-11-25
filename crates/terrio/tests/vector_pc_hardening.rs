// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Regression tests for malformed vector, GeoPackage, point cloud, and tile inputs.

use flate2::Compression;
use flate2::write::GzEncoder;
use std::io::Write;
use terrio::memory::{open_vector_bytes, open_vector_or_geometries};
use terrio::pointcloud::{open_points, open_points_bytes};

/// Opens a table that terrio's native TAB reader refuses and returns the refusal. With the `gdal`
/// feature the refusal falls back to GDAL, which reads a seamless table as a plain table, so only
/// the absence of a panic or stack overflow is checked and `None` is returned.
fn native_tab_refusal(path: &std::path::Path) -> Option<terrio::Error> {
    let result = terrio::open_vector(path.to_str().unwrap(), &Default::default());
    if cfg!(feature = "gdal") { None } else { Some(result.unwrap_err()) }
}

#[test]
fn tab_dat_out_of_bounds_header_and_zero_record_len_return_error() {
    let tab = b"!table\n!version 300\n!charset Neutral\nDefinition Table\n  Type NATIVE Charset \"Neutral\"\n  Fields 0\n";

    // header_len = 64 > dat.len() (32) must return Err, not panic.
    let mut bad_header = vec![0u8; 32];
    bad_header[8..10].copy_from_slice(&64u16.to_le_bytes());
    bad_header[10..12].copy_from_slice(&1u16.to_le_bytes());
    let dir = tempfile::tempdir().unwrap();
    let tab_path = dir.path().join("bad_hdr.tab");
    std::fs::write(&tab_path, tab).unwrap();
    std::fs::write(dir.path().join("bad_hdr.dat"), &bad_header).unwrap();
    let _ = native_tab_refusal(&tab_path);

    // records = 1, header_len = 32, record_len = 0 must return Err, not panic on rec[0].
    let mut bad_rec = vec![0u8; 32];
    bad_rec[4..8].copy_from_slice(&1u32.to_le_bytes());
    bad_rec[8..10].copy_from_slice(&32u16.to_le_bytes());
    bad_rec[10..12].copy_from_slice(&0u16.to_le_bytes());
    let tab_path2 = dir.path().join("bad_rec.tab");
    std::fs::write(&tab_path2, tab).unwrap();
    std::fs::write(dir.path().join("bad_rec.dat"), &bad_rec).unwrap();
    let _ = native_tab_refusal(&tab_path2);
}

#[test]
fn tab_seamless_rejects_parent_traversal_absolute_paths_and_cycles() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    std::fs::create_dir_all(&sub).unwrap();

    // Minimal 1-field Table seamless .tab + .dat helper (sized dynamically for long TMPDIR paths).
    let make_seamless = |path: &std::path::Path, target: &str| {
        let field_len = target.len().clamp(64, 254);
        let rec_len = 1 + field_len;
        let tab = format!(
            "!table\n!version 300\n!charset Neutral\nDefinition Table\n  Type NATIVE Charset \"Neutral\"\n  Fields 1\n    Table Char ({field_len}) ;\nbegin_metadata\n\"\\IsSeamless\" = \"TRUE\"\nend_metadata\n"
        );
        let mut dat = vec![0u8; 32 + 32 + 1 + rec_len];
        dat[4..8].copy_from_slice(&1u32.to_le_bytes()); // 1 record
        dat[8..10].copy_from_slice(&65u16.to_le_bytes()); // header_len = 32 + 32 + 1 (0x0d)
        dat[10..12].copy_from_slice(&(rec_len as u16).to_le_bytes()); // record_len = 1 + field_len
        dat[32..37].copy_from_slice(b"Table");
        dat[32 + 11] = b'C';
        dat[32 + 16] = field_len as u8;
        dat[64] = 0x0d;
        dat[65] = b' ';
        let bytes = target.as_bytes();
        dat[66..66 + bytes.len()].copy_from_slice(bytes);
        std::fs::write(path, tab).unwrap();
        std::fs::write(path.with_extension("dat"), &dat).unwrap();
    };

    // Create an actual valid sibling table outside `sub/` so containment errors are pinned
    // instead of failing merely because the target file is missing.
    let outside_tab = dir.path().join("outside.tab");
    {
        let tab = "!table\n!version 300\n!charset Neutral\nDefinition Table\n  Type NATIVE Charset \"Neutral\"\n  Fields 1\n    Name Char (8) ;\n";
        let mut dat = vec![0u8; 32 + 32 + 1 + 9];
        dat[4..8].copy_from_slice(&1u32.to_le_bytes());
        dat[8..10].copy_from_slice(&65u16.to_le_bytes());
        dat[10..12].copy_from_slice(&9u16.to_le_bytes());
        dat[32..36].copy_from_slice(b"Name");
        dat[32 + 11] = b'C';
        dat[32 + 16] = 8;
        dat[64] = 0x0d;
        dat[65] = b' ';
        dat[66..70].copy_from_slice(b"test");
        std::fs::write(&outside_tab, tab).unwrap();
        std::fs::write(outside_tab.with_extension("dat"), &dat).unwrap();
    }
    assert!(terrio::open_vector(outside_tab.to_str().unwrap(), &Default::default()).is_ok());

    // Self-referential cycle must hit cycle detection, not the depth cap or stack overflow.
    let self_tab = sub.join("loop.tab");
    make_seamless(&self_tab, "loop.tab");
    if let Some(err) = native_tab_refusal(&self_tab) {
        assert!(err.to_string().contains("cyclic"), "expected cycle error, got: {err}");
    }

    // Parent directory traversal must be refused by the containment check.
    let parent_tab = sub.join("escape.tab");
    make_seamless(&parent_tab, "../outside.tab");
    if let Some(err) = native_tab_refusal(&parent_tab) {
        assert!(err.to_string().contains("escapes"), "expected escape error, got: {err}");
    }

    // Absolute path to an existing valid table must also be refused by the containment check.
    let abs_tab = sub.join("abs.tab");
    make_seamless(&abs_tab, outside_tab.to_str().unwrap());
    if let Some(err) = native_tab_refusal(&abs_tab) {
        assert!(err.to_string().contains("escapes"), "expected escape error, got: {err}");
    }

    // A flat seamless table referencing >1024 parts still opens cleanly under the 10_000 budget.
    let flat_dir = dir.path().join("flat");
    std::fs::create_dir_all(&flat_dir).unwrap();
    let flat_leaf = flat_dir.join("leaf.tab");
    std::fs::write(&flat_leaf, std::fs::read(&outside_tab).unwrap()).unwrap();
    std::fs::write(flat_leaf.with_extension("dat"), std::fs::read(outside_tab.with_extension("dat")).unwrap()).unwrap();
    let flat_tab = flat_dir.join("flat.tab");
    let flat_rows = 1050usize;
    let tab_hdr = "!table\n!version 300\n!charset Neutral\nDefinition Table\n  Type NATIVE Charset \"Neutral\"\n  Fields 1\n    Table Char (16) ;\nbegin_metadata\n\"\\IsSeamless\" = \"TRUE\"\nend_metadata\n";
    let mut flat_dat = vec![0u8; 32 + 32 + 1 + flat_rows * 17];
    flat_dat[4..8].copy_from_slice(&(flat_rows as u32).to_le_bytes());
    flat_dat[8..10].copy_from_slice(&65u16.to_le_bytes());
    flat_dat[10..12].copy_from_slice(&17u16.to_le_bytes());
    flat_dat[32..37].copy_from_slice(b"Table");
    flat_dat[32 + 11] = b'C';
    flat_dat[32 + 16] = 16;
    flat_dat[64] = 0x0d;
    for row in 0..flat_rows {
        let off = 65 + row * 17;
        flat_dat[off] = b' ';
        flat_dat[off + 1..off + 1 + b"leaf.tab".len()].copy_from_slice(b"leaf.tab");
    }
    std::fs::write(&flat_tab, tab_hdr).unwrap();
    std::fs::write(flat_tab.with_extension("dat"), &flat_dat).unwrap();
    assert!(terrio::open_vector(flat_tab.to_str().unwrap(), &Default::default()).is_ok());

    // Exponentially branching DAG (14 levels x 2 parts = 16383 > 10_000 reads) must hit ResourceLimit.
    let dag_dir = dir.path().join("dag");
    std::fs::create_dir_all(&dag_dir).unwrap();
    let leaf = dag_dir.join("l14.tab");
    std::fs::write(&leaf, std::fs::read(&outside_tab).unwrap()).unwrap();
    std::fs::write(leaf.with_extension("dat"), std::fs::read(outside_tab.with_extension("dat")).unwrap()).unwrap();
    for lvl in (0..14).rev() {
        let p = dag_dir.join(format!("l{lvl}.tab"));
        let next = format!("l{}.tab", lvl + 1);
        let tab = "!table\n!version 300\n!charset Neutral\nDefinition Table\n  Type NATIVE Charset \"Neutral\"\n  Fields 1\n    Table Char (16) ;\nbegin_metadata\n\"\\IsSeamless\" = \"TRUE\"\nend_metadata\n";
        let mut dat = vec![0u8; 32 + 32 + 1 + 2 * 17];
        dat[4..8].copy_from_slice(&2u32.to_le_bytes());
        dat[8..10].copy_from_slice(&65u16.to_le_bytes());
        dat[10..12].copy_from_slice(&17u16.to_le_bytes());
        dat[32..37].copy_from_slice(b"Table");
        dat[32 + 11] = b'C';
        dat[32 + 16] = 16;
        dat[64] = 0x0d;
        for row in 0..2 {
            let off = 65 + row * 17;
            dat[off] = b' ';
            dat[off + 1..off + 1 + next.len()].copy_from_slice(next.as_bytes());
        }
        std::fs::write(&p, tab).unwrap();
        std::fs::write(p.with_extension("dat"), &dat).unwrap();
    }
    if let Some(err) = native_tab_refusal(&dag_dir.join("l0.tab")) {
        assert!(matches!(err, terrio::Error::ResourceLimit(_)), "expected ResourceLimit for DAG fan-out, got: {err:?}");
    }
}

#[test]
fn deeply_nested_xml_wkt_and_mif_return_errors_without_stack_overflow() {
    // Deeply nested KML (>1024 levels) hits the nesting ResourceLimit cleanly.
    let deep_kml = format!("<kml>{}{}</kml>", "<a>".repeat(1500), "</a>".repeat(1500));
    let kml_err = open_vector_bytes(deep_kml.into_bytes(), "deep.kml", &Default::default()).unwrap_err();
    assert!(matches!(kml_err, terrio::Error::ResourceLimit(_)), "expected ResourceLimit, got: {kml_err:?}");

    // WKT GEOMETRYCOLLECTION within the 64-level cap succeeds; exceeding it fails cleanly.
    let ok_wkt = format!("{}POINT(0 0){}", "GEOMETRYCOLLECTION(".repeat(60), ")".repeat(60));
    assert!(open_vector_or_geometries(&ok_wkt, &Default::default()).is_ok());
    let deep_wkt = format!("{}POINT(0 0){}", "GEOMETRYCOLLECTION(".repeat(65), ")".repeat(65));
    assert!(open_vector_or_geometries(&deep_wkt, &Default::default()).is_err());

    // MIF Collection within the 64-level cap succeeds; exceeding it hits the depth cap cleanly.
    let ok_mif = format!("Version 300\nColumns 0\nData\n{}Point 0 0\n", "Collection 1\n".repeat(60));
    assert!(open_vector_bytes(ok_mif.into_bytes(), "ok.mif", &Default::default()).is_ok());
    let deep_mif = format!("Version 300\nColumns 0\nData\n{}Point 0 0\n", "Collection 1\n".repeat(65));
    // With the `gdal` feature the native refusal falls back to GDAL, which reads the collection.
    let deep_mif = open_vector_bytes(deep_mif.into_bytes(), "deep.mif", &Default::default());
    assert!(cfg!(feature = "gdal") || deep_mif.is_err());
}

#[test]
fn ply_zero_property_vertex_element_returns_error_immediately() {
    let ply = b"ply\nformat ascii 1.0\nelement vertex 18446744073709551615\nend_header\n".to_vec();
    let Err(err) = open_points_bytes(ply, "empty_props.ply") else { panic!("expected error") };
    let err = err.to_string();
    assert!(err.contains("no scalar properties"), "got: {err}");
}

#[test]
fn ept_zero_span_and_oversized_zstd_node_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let ept = serde_json::json!({
        "bounds": [0.0, 0.0, 0.0, 10.0, 10.0, 10.0],
        "boundsConforming": [0.0, 0.0, 0.0, 10.0, 10.0, 10.0],
        "dataType": "binary",
        "hierarchyType": "json",
        "points": 1,
        "span": 0,
        "version": "1.0.0",
        "schema": [
            {"name": "X", "type": "floating", "size": 8},
            {"name": "Y", "type": "floating", "size": 8},
            {"name": "Z", "type": "floating", "size": 8}
        ]
    });
    let ept_path = dir.path().join("ept.json");
    std::fs::write(&ept_path, serde_json::to_vec(&ept).unwrap()).unwrap();
    let Err(err) = open_points(ept_path.to_str().unwrap()) else { panic!("expected error") };
    let err = err.to_string();
    assert!(err.contains("span"), "got: {err}");

    // An EPT zstandard node whose hierarchy declares >256 MiB of records returns ResourceLimit.
    let dir2 = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir2.path().join("ept-hierarchy")).unwrap();
    std::fs::create_dir_all(dir2.path().join("ept-data")).unwrap();
    let ept_zst = serde_json::json!({
        "bounds": [0.0, 0.0, 0.0, 10.0, 10.0, 10.0],
        "boundsConforming": [0.0, 0.0, 0.0, 10.0, 10.0, 10.0],
        "dataType": "zstandard",
        "hierarchyType": "json",
        "points": 20_000_000,
        "span": 256,
        "version": "1.0.0",
        "schema": [
            {"name": "X", "type": "floating", "size": 8},
            {"name": "Y", "type": "floating", "size": 8},
            {"name": "Z", "type": "floating", "size": 8}
        ]
    });
    let ept_zst_path = dir2.path().join("ept.json");
    std::fs::write(&ept_zst_path, serde_json::to_vec(&ept_zst).unwrap()).unwrap();
    std::fs::write(dir2.path().join("ept-hierarchy/0-0-0-0.json"), r#"{"0-0-0-0": 20000000}"#).unwrap();
    std::fs::write(dir2.path().join("ept-data/0-0-0-0.zst"), b"stub").unwrap();
    let mut ds = open_points(ept_zst_path.to_str().unwrap()).unwrap();
    let err = ds.read(&Default::default()).unwrap_err();
    assert!(matches!(err, terrio::Error::ResourceLimit(_)), "expected ResourceLimit, got: {err:?}");
}

#[test]
fn mvt_gzip_bomb_hits_decoded_limit_and_does_not_oom() {
    // Build a >64 MiB zero payload compressed with gzip (~70 KiB compressed).
    let mut enc = GzEncoder::new(Vec::new(), Compression::best());
    let chunk = vec![0u8; 1024 * 1024];
    for _ in 0..70 {
        enc.write_all(&chunk).unwrap();
    }
    let bomb = enc.finish().unwrap();
    assert!(bomb.len() < 1024 * 1024);
    let err = open_vector_bytes(bomb, "0_0_0.mvt", &Default::default()).unwrap_err();
    assert!(
        matches!(err, terrio::Error::DecodedLimitExceeded { .. }) && err.to_string().contains("MVT tile payload"),
        "expected DecodedLimitExceeded for 64 MiB MVT bomb, got: {err:?}"
    );
}
