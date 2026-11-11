// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! Append policy is checked before locks, tile decoding or interrupted-update recovery.
#![cfg(all(feature = "pointcloud", not(target_family = "wasm")))]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use terrio::crs::SpatialRef;
use terrio::pointcloud::{self, ConvertOptions, Dimension, EptDataType, PointCloud, PointFormat, PointWriteOptions, Values};

fn dataset(dir: &Path, name: &str, count: usize, codec: EptDataType) -> PathBuf {
    let points = PointCloud::new(
        ["x", "y", "z"]
            .into_iter()
            .enumerate()
            .map(|(axis, name)| Dimension::new(name, Values::F64((0..count).map(|i| ((i * 37 + axis * 11) % 1000) as f64).collect())))
            .collect(),
        SpatialRef::from_epsg(32633).unwrap(),
    );
    let path = dir.join(name);
    pointcloud::write_points(
        &points,
        path.to_str().unwrap(),
        &PointWriteOptions {
            format: Some(PointFormat::Ept),
            ept_data_type: codec,
            ept_bounds: Some([0.0, 0.0, 0.0, 1000.0, 1000.0, 1000.0]),
            ..Default::default()
        },
    )
    .unwrap();
    path.join("ept.json")
}

fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                // Record directories too, so deleting an empty staging directory is detected.
                files.insert(path.strip_prefix(root).unwrap().to_path_buf(), Vec::new());
                visit(root, &path, files);
            } else {
                files.insert(path.strip_prefix(root).unwrap().to_path_buf(), std::fs::read(&path).unwrap());
            }
        }
    }
    let mut files = BTreeMap::new();
    visit(dir, dir, &mut files);
    files
}

fn interrupted_update(target: &Path, state: &str) {
    if state == "none" {
        return;
    }
    let stage = target.parent().unwrap().join(".terrio-ept-append-0");
    std::fs::create_dir(&stage).unwrap();
    if state == "committed" {
        let mut metadata: serde_json::Value = serde_json::from_slice(&std::fs::read(target).unwrap()).unwrap();
        metadata["recovered"] = true.into();
        std::fs::write(stage.join("ept.json"), serde_json::to_vec(&metadata).unwrap()).unwrap();
        std::fs::write(stage.join("COMMITTED"), b"").unwrap();
    } else {
        std::fs::write(stage.join("unfinished"), b"must remain after refusal").unwrap();
    }
}

fn append(target: &Path, source: &Path, convert: bool) -> terrio::Result<()> {
    let (target, source) = (target.to_str().unwrap(), source.to_str().unwrap());
    if convert {
        pointcloud::convert(source, target, &ConvertOptions { append: true, ..Default::default() })
    } else {
        pointcloud::append_points(target, &[source], &Default::default())
    }
}

fn refused(target: &Path, source: &Path, convert: bool) {
    let error = append(target, source, convert).unwrap_err();
    assert!(matches!(error, terrio::Error::Unsupported(_)), "{error:?}");
    assert!(error.to_string().contains("driver is not allowed"), "{error}");
}

#[test]
fn append_respects_driver_policy_before_any_dataset_changes() {
    // Exercise the process option in children, without mutating this process's environment.
    if let Ok(target) = std::env::var("TERRIO_TEST_EPT_TARGET") {
        let source = std::env::var("TERRIO_TEST_EPT_SOURCE").unwrap();
        refused(Path::new(&target), Path::new(&source), std::env::var("TERRIO_TEST_EPT_CONVERT").unwrap() == "true");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let source = dataset(dir.path(), "source", 50, EptDataType::Binary);
    let mut case = 0;
    for allowed in ["EPT", "EPT,LAS", "EPT,COPC", "LAS"] {
        for state in ["none", "pending", "committed"] {
            for convert in [false, true] {
                for process in [false, true] {
                    case += 1;
                    let target = dataset(dir.path(), &format!("target-{case}"), 100, EptDataType::Laszip);
                    interrupted_update(&target, state);
                    let before = snapshot(target.parent().unwrap());
                    if process {
                        let output = std::process::Command::new(std::env::current_exe().unwrap())
                            .args(["--exact", "append_respects_driver_policy_before_any_dataset_changes", "--nocapture"])
                            .env("TERRIO_ALLOWED_DRIVERS", allowed)
                            .env("TERRIO_TEST_EPT_TARGET", &target)
                            .env("TERRIO_TEST_EPT_SOURCE", &source)
                            .env("TERRIO_TEST_EPT_CONVERT", convert.to_string())
                            .output()
                            .unwrap();
                        assert!(
                            output.status.success(),
                            "{}\n{}",
                            String::from_utf8_lossy(&output.stdout),
                            String::from_utf8_lossy(&output.stderr)
                        );
                    } else {
                        terrio::config::push_scope([("TERRIO_ALLOWED_DRIVERS".into(), Some(allowed.into()))].into());
                        let result = append(&target, &source, convert);
                        terrio::config::pop_scope();
                        assert!(matches!(result, Err(terrio::Error::Unsupported(_))), "{result:?}");
                    }
                    assert_eq!(snapshot(target.parent().unwrap()), before, "{allowed}, {state}, convert={convert}, process={process}");
                    assert_eq!(pointcloud::read_points(target.to_str().unwrap(), &Default::default()).unwrap().len(), 100);
                }
            }
        }
    }
    // Allowed reads still append and recover normally for all supported tile codecs.
    for (codec, allowed) in [(EptDataType::Laszip, "EPT,LAZ"), (EptDataType::Binary, "EPT"), (EptDataType::Zstandard, "EPT")] {
        for state in ["none", "pending", "committed"] {
            case += 1;
            let target = dataset(dir.path(), &format!("target-{case}"), 100, codec);
            interrupted_update(&target, state);
            terrio::config::push_scope([("TERRIO_ALLOWED_DRIVERS".into(), Some(allowed.into()))].into());
            let result = append(&target, &source, false);
            terrio::config::pop_scope();
            result.unwrap();
            assert_eq!(pointcloud::read_points(target.to_str().unwrap(), &Default::default()).unwrap().len(), 150);
            assert!(!target.parent().unwrap().join(".terrio-ept-append-0").exists());
        }
    }
}
