// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! CityGML format options are checked before an unrelated conversion opens its input.
use std::process::Command;

#[test]
fn citygml_version_is_rejected_on_unrelated_formats_before_io() {
    let folder = tempfile::tempdir().unwrap();
    for (name, format) in [("output.tif", None), ("misleading.citygml", Some("GTiff")), ("output.geojson", Some("GeoJSON"))] {
        let output = folder.path().join(name);
        let mut command = Command::new(env!("CARGO_BIN_EXE_terrio"));
        command.arg("convert").arg(folder.path().join("missing.tif")).arg(&output).args(["--citygml-version", "2.0"]);
        if let Some(format) = format {
            command.args(["--format", format]);
        }
        let result = command.output().unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("applies to CityGML"));
        assert!(!output.exists());
    }
}
