//! Vector clip CLI parsing, output safety and automatic MCP discovery.
use std::process::Command;

fn run(args: &[&str], path: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_terrio")).args(args).current_dir(path).output().unwrap()
}
#[test]
fn clip_bbox_and_geometry_preserve_values_and_refuse_accidental_overwrites() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path();
    std::fs::write(path.join("input.geojson"),r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"name":"road"},"geometry":{"type":"LineString","coordinates":[[-1,1],[3,1]]}},{"type":"Feature","properties":{"name":"outside"},"geometry":{"type":"Point","coordinates":[5,5]}}]}"#).unwrap();
    let args = ["vector-clip", "input.geojson", "out.geojson", "--bbox", "0,0,2,2"];
    let out = run(&args, path);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let content = std::fs::read(path.join("out.geojson")).unwrap();
    let result: serde_json::Value = serde_json::from_slice(&content).unwrap();
    assert_eq!(result["features"].as_array().unwrap().len(), 1);
    assert_eq!(result["features"][0]["properties"]["name"], "road");
    assert_eq!(result["features"][0]["geometry"]["coordinates"], serde_json::json!([[0.0, 1.0], [2.0, 1.0]]));
    assert!(!run(&args, path).status.success());
    assert_eq!(std::fs::read(path.join("out.geojson")).unwrap(), content);
    let invalid = ["vector-clip", "input.geojson", "invalid.geojson", "--bbox", "0,0,0,1"];
    assert!(!run(&invalid, path).status.success());
    assert!(!path.join("invalid.geojson").exists());
    let missing = ["vector-clip", "input.geojson", "missing.geojson", "--bbox", "0,0,2,2", "--layer", "absent"];
    assert!(!run(&missing, path).status.success());
    assert!(!path.join("missing.geojson").exists());
    let conflicting =
        ["vector-clip", "input.geojson", "conflict.geojson", "--bbox", "0,0,2,2", "--geometry", "POLYGON ((0 0,2 0,2 2,0 2,0 0))"];
    assert!(!run(&conflicting, path).status.success());
    assert!(!path.join("conflict.geojson").exists());
}

#[test]
fn vector_clip_is_an_mcp_tool() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;
    let mut child = Command::new(env!("CARGO_BIN_EXE_terrio")).arg("mcp").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "{}", serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).unwrap();
    let line = BufReader::new(child.stdout.take().unwrap()).lines().next().unwrap().unwrap();
    let response: serde_json::Value = serde_json::from_str(&line).unwrap();
    let tool = response["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == "vector-clip").unwrap();
    assert!(tool["description"].as_str().unwrap().contains("--geometry-crs"));
    assert_eq!(tool["annotations"]["readOnlyHint"], false);
    drop(stdin);
    assert!(child.wait().unwrap().success());
}
