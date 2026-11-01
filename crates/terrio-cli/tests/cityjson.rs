// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! CityJSON format options are checked before an unrelated conversion opens its input.
use std::process::Command;

#[test]
fn cityjson_version_is_rejected_on_unrelated_formats_before_io() {
    let folder = tempfile::tempdir().unwrap();
    for (name, format) in [("output.tif", None), ("misleading.city.json", Some("GTiff")), ("output.geojson", Some("GeoJSON"))] {
        let output = folder.path().join(name);
        let mut command = Command::new(env!("CARGO_BIN_EXE_terrio"));
        command.arg("convert").arg(folder.path().join("missing.tif")).arg(&output).args(["--cityjson-version", "1.1"]);
        if let Some(format) = format {
            command.args(["--format", format]);
        }
        let result = command.output().unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("applies to CityJSON"));
        assert!(!output.exists());
    }
}

#[test]
fn cityjson_content_in_json_refuses_update_modes_before_modifying_output() {
    let folder = tempfile::tempdir().unwrap();
    let input = folder.path().join("source.geojson");
    std::fs::write(&input, br#"{"type":"FeatureCollection","features":[]}"#).unwrap();
    let output = folder.path().join("existing.json");
    let bytes = br#"{"type":"CityJSON","version":"2.0","transform":{"scale":[1,1,1],"translate":[0,0,0]},"CityObjects":{"keep":{"type":"GenericCityObject"}},"vertices":[]}"#;
    std::fs::write(&output, bytes).unwrap();
    for mode in ["--append", "--overwrite"] {
        let result = Command::new(env!("CARGO_BIN_EXE_terrio")).arg("convert").arg(&input).arg(&output).arg(mode).output().unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("CityJSON has no persistent layer names"));
        assert_eq!(std::fs::read(&output).unwrap(), bytes);
    }
}
