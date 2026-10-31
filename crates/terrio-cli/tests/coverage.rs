//! Coverage CLI behavior and automatic MCP discovery.
use std::process::{Command, Stdio};

#[test]
fn coverage_reports_original_edges_and_preserves_valid_correspondence() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("input.geojson");
    std::fs::write(&source, r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"name":"west"},"geometry":{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,2],[0,2],[0,0]]]}},{"type":"Feature","properties":{"name":"east"},"geometry":{"type":"Polygon","coordinates":[[[1.1,0],[2.1,0],[2.1,2],[1.1,2],[1.1,0]]]}}]}"#).unwrap();
    let run = |output: &str, options: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_terrio"))
            .current_dir(directory.path())
            .args(["check-coverage", "input.geojson", output])
            .args(options)
            .output()
            .unwrap()
    };
    let result = run("invalid.geojson", &["--maximum-gap-width", "0.2"]);
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let output: serde_json::Value = serde_json::from_slice(&std::fs::read(directory.path().join("invalid.geojson")).unwrap()).unwrap();
    assert_eq!(output["features"].as_array().unwrap().len(), 2);
    assert!(output["features"][0]["properties"].as_object().unwrap().is_empty());
    assert_eq!(output["features"][0]["geometry"]["type"], "MultiLineString");
    let result = run("all.geojson", &["--include-valid"]);
    assert!(result.status.success());
    let output: serde_json::Value = serde_json::from_slice(&std::fs::read(directory.path().join("all.geojson")).unwrap()).unwrap();
    assert_eq!(output["features"].as_array().unwrap().len(), 2);
    for feature in output["features"].as_array().unwrap() {
        assert!(feature["geometry"]["coordinates"].as_array().unwrap().is_empty());
    }
    assert!(!run("negative.geojson", &["--maximum-gap-width", "-1"]).status.success());
    assert!(!directory.path().join("negative.geojson").exists());
}

#[test]
fn coverage_is_an_mcp_tool() {
    use std::io::{BufRead, BufReader, Write};
    let mut child = Command::new(env!("CARGO_BIN_EXE_terrio")).arg("mcp").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    writeln!(input, "{}", serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).unwrap();
    let line = BufReader::new(child.stdout.take().unwrap()).lines().next().unwrap().unwrap();
    let response: serde_json::Value = serde_json::from_str(&line).unwrap();
    let tool = response["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == "check-coverage").unwrap();
    assert!(tool["description"].as_str().unwrap().contains("--maximum-gap-width"));
    assert_eq!(tool["annotations"]["readOnlyHint"], false);
    drop(input);
    assert!(child.wait().unwrap().success());
}
