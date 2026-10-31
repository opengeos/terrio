//! Layer algebra output safety and MCP discovery.
use std::process::{Command, Stdio};
#[test]
fn operations_and_preflight_errors_preserve_existing_outputs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    for (name, coordinates) in [("a", serde_json::json!([[0, 0], [3, 3]])), ("b", serde_json::json!([[0, 3], [3, 0]]))] {
        std::fs::write(path.join(format!("{name}.geojson")),serde_json::to_vec(&serde_json::json!({"type":"FeatureCollection","features":[{"type":"Feature","properties":{"id":1},"geometry":{"type":"LineString","coordinates":coordinates}}]})).unwrap()).unwrap();
    }
    let run = |args: &[&str]| Command::new(env!("CARGO_BIN_EXE_terrio")).current_dir(path).args(args).output().unwrap();
    for (op, count) in [("intersection", 1), ("union", 3), ("sym-difference", 2), ("identity", 2), ("update", 2), ("clip", 1), ("erase", 1)]
    {
        let name = format!("{op}.geojson");
        let args = ["layer-algebra", op, "a.geojson", "b.geojson", &name];
        let result = run(&args);
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        let bytes = std::fs::read(path.join(&name)).unwrap();
        let data: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(data["features"].as_array().unwrap().len(), count);
        assert!(!run(&args).status.success());
        assert_eq!(std::fs::read(path.join(name)).unwrap(), bytes);
    }
    let before = std::fs::read(path.join("clip.geojson")).unwrap();
    assert!(
        !run(&["layer-algebra", "clip", "a.geojson", "b.geojson", "clip.geojson", "--overwrite", "--method-layer", "missing"])
            .status
            .success()
    );
    assert_eq!(std::fs::read(path.join("clip.geojson")).unwrap(), before);
    assert!(!run(&["layer-algebra", "invalid", "a.geojson", "b.geojson", "invalid.geojson"]).status.success());
    assert!(!path.join("invalid.geojson").exists());
}
#[test]
fn layer_algebra_is_an_mcp_tool() {
    use std::io::{BufRead, BufReader, Write};
    let mut child = Command::new(env!("CARGO_BIN_EXE_terrio")).arg("mcp").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    writeln!(child.stdin.take().unwrap(), "{}", serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).unwrap();
    let line = BufReader::new(child.stdout.take().unwrap()).lines().next().unwrap().unwrap();
    let data: serde_json::Value = serde_json::from_str(&line).unwrap();
    let tool = data["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == "layer-algebra").unwrap();
    assert!(tool["description"].as_str().unwrap().contains("--method-layer"));
    assert_eq!(tool["annotations"]["readOnlyHint"], false);
    child.kill().unwrap();
    child.wait().unwrap();
}
