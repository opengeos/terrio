//! Pipeline output safety, saved-definition execution and MCP discovery.
use std::process::{Command, Stdio};

#[test]
fn saved_pipeline_runs_and_preflight_preserves_outputs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    std::fs::write(path.join("input with spaces.geojson"), r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"score":3},"geometry":{"type":"Point","coordinates":[1,2]}},{"type":"Feature","properties":{"score":0},"geometry":{"type":"Point","coordinates":[3,4]}}]}"#).unwrap();
    let run = |args: &[&str]| Command::new(env!("CARGO_BIN_EXE_terrio")).current_dir(path).args(args).output().unwrap();
    let result =
        run(&["pipeline", "--save", "plan.gdalg.json", "read", "input with spaces.geojson", "!", "filter", "--where", "score > 1"]);
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let args = ["pipeline", "--load", "plan.gdalg.json", "--output", "result.geojson"];
    let result = run(&args);
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let before = std::fs::read(path.join("result.geojson")).unwrap();
    let data: serde_json::Value = serde_json::from_slice(&before).unwrap();
    assert_eq!(data["features"].as_array().unwrap().len(), 1);
    assert!(!run(&args).status.success());
    assert_eq!(std::fs::read(path.join("result.geojson")).unwrap(), before);
    let result = run(&[
        "pipeline",
        "read",
        "input with spaces.geojson",
        "!",
        "materialize",
        "--output",
        "checkpoint.geojson",
        "!",
        "write",
        "result.geojson",
    ]);
    assert!(!result.status.success());
    assert!(!path.join("checkpoint.geojson").exists());
    assert_eq!(std::fs::read(path.join("result.geojson")).unwrap(), before);
}

#[test]
fn pipeline_is_an_mcp_tool_with_write_annotations() {
    use std::io::{BufRead, BufReader, Write};
    let mut child = Command::new(env!("CARGO_BIN_EXE_terrio")).arg("mcp").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    writeln!(child.stdin.take().unwrap(), "{}", serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).unwrap();
    let line = BufReader::new(child.stdout.take().unwrap()).lines().next().unwrap().unwrap();
    let data: serde_json::Value = serde_json::from_str(&line).unwrap();
    let tool = data["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == "pipeline").unwrap();
    assert!(tool["description"].as_str().unwrap().contains("--load"));
    assert!(tool["description"].as_str().unwrap().split_whitespace().collect::<Vec<_>>().join(" ").contains("write and overwrite options"));
    assert_eq!(tool["annotations"]["readOnlyHint"], false);
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn loaded_definition_resolves_only_input_paths_against_its_directory() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    std::fs::create_dir(root.join("definitions")).unwrap();
    std::fs::write(
        root.join("definitions/source.geojson"),
        r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[1,2]}}]}"#,
    )
    .unwrap();
    std::fs::write(
        root.join("definitions/plan.gdalg.json"),
        serde_json::to_vec(&serde_json::json!({
            "type":"gdal_streamed_alg", "relative_paths_relative_to_this_file":true,
            "command_line":"gdal vector pipeline read source.geojson ! materialize --output checkpoint.geojson"
        }))
        .unwrap(),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_terrio"))
        .current_dir(root)
        .args(["pipeline", "--load", "definitions/plan.gdalg.json", "--output", "result.geojson"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(root.join("checkpoint.geojson").exists());
    assert!(!root.join("definitions/checkpoint.geojson").exists());
    assert_eq!(std::fs::read(root.join("checkpoint.geojson")).unwrap(), std::fs::read(root.join("result.geojson")).unwrap());
}
