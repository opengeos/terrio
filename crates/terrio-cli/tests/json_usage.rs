//! The CLI catalog is metadata-only and shares the command definitions used by MCP.
use serde_json::{Value, json};
use std::process::{Command, Output, Stdio};

fn run(args: &[&str], cwd: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_terrio")).args(args).current_dir(cwd).stdin(Stdio::null()).output().unwrap()
}

#[test]
fn root_nested_alias_and_variadic_catalogs_need_no_datasets() {
    let dir = tempfile::tempdir().unwrap();
    for (args, path) in [
        (vec!["--json-usage"], vec!["terrio"]),
        (vec!["warp", "--json-usage"], vec!["terrio", "warp"]),
        (vec!["--json-usage", "warp"], vec!["terrio", "warp"]),
        (vec!["vsi", "ls", "--json-usage"], vec!["terrio", "vsi", "list"]),
        (vec!["vsi", "sync", "--json-usage"], vec!["terrio", "vsi", "sync"]),
        (vec!["vsi", "sozip", "create", "--json-usage"], vec!["terrio", "vsi", "sozip", "create"]),
        (vec!["warp", "missing.tif", "output.tif", "--json-usage"], vec!["terrio", "warp"]),
        (vec!["mosaic", "a.tif", "b.tif", "output.tif", "--json-usage"], vec!["terrio", "mosaic"]),
        (vec!["calc", "-A", "missing.tif", "--json-usage"], vec!["terrio", "calc"]),
    ] {
        let output = run(&args, dir.path());
        assert!(output.status.success(), "{args:?}: {}", String::from_utf8_lossy(&output.stderr));
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["schema_version"], "0.1");
        assert_eq!(value["command"]["path"], json!(path));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}

#[test]
fn metadata_never_deletes_its_positional_input() {
    let dir = tempfile::tempdir().unwrap();
    let data = br#"{"type":"FeatureCollection","features":[]}"#;
    std::fs::write(dir.path().join("keep.geojson"), data).unwrap();
    let output = run(&["dataset-delete", "keep.geojson", "--json-usage"], dir.path());
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(std::fs::read(dir.path().join("keep.geojson")).unwrap(), data);
    assert_eq!(serde_json::from_slice::<Value>(&output.stdout).unwrap()["command"]["name"], "dataset-delete");
}

#[test]
fn literal_flag_values_follow_normal_cli_parsing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("--json-usage"), b"literal dataset name").unwrap();
    let output = run(&["vsi", "list", "--", "--json-usage"], dir.path());
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("schema_version"));
    let output = run(&["warp", "--srcnodata", "--json-usage"], dir.path());
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    // Excluded commands never enter the MCP server loop for a metadata request.
    let output = run(&["mcp", "--json-usage"], dir.path());
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn invalid_catalog_paths_report_usage_errors() {
    let dir = tempfile::tempdir().unwrap();
    for args in [vec!["--json-usage", "not-a-command"], vec!["--json-usage", "vsi", "not-a-command"]] {
        let output = run(&args, dir.path());
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }
}
