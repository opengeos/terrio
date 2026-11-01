//! Shared-edge simplification CLI safety and MCP discovery.
use std::process::{Command, Stdio};

#[test]
fn simplify_shared_boundaries_and_preserve_outputs_when_preflight_fails() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    std::fs::write(path.join("input.geojson"),r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"name":"west"},"geometry":{"type":"Polygon","coordinates":[[[0,0],[2,0],[1.8,1],[2.2,2],[1.8,3],[2,4],[0,4],[0,0]]]}},{"type":"Feature","properties":{"name":"east"},"geometry":{"type":"Polygon","coordinates":[[[2,0],[4,0],[4,4],[2,4],[1.8,3],[2.2,2],[1.8,1],[2,0]]]}}]}"#).unwrap();
    let run = |options: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_terrio"))
            .current_dir(path)
            .args(["simplify-coverage", "input.geojson", "out.geojson"])
            .args(options)
            .output()
            .unwrap()
    };
    let result = run(&["--tolerance", "1", "--preserve-boundary"]);
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let bytes = std::fs::read(path.join("out.geojson")).unwrap();
    let data: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(data["features"].as_array().unwrap().len(), 2);
    assert_eq!(data["features"][0]["properties"]["name"], "west");
    assert!(data["features"].as_array().unwrap().iter().all(|f| f["geometry"]["coordinates"][0].as_array().unwrap().len() == 5));
    for options in [vec!["--tolerance", "-1", "--overwrite"], vec!["--tolerance", "1", "--active-layer", "absent", "--overwrite"]] {
        assert!(!run(&options).status.success());
        assert_eq!(std::fs::read(path.join("out.geojson")).unwrap(), bytes);
    }
}

#[test]
fn simplify_coverage_is_an_mcp_tool() {
    use std::io::{BufRead, BufReader, Write};
    let mut child = Command::new(env!("CARGO_BIN_EXE_terrio")).arg("mcp").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    writeln!(input, "{}", serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).unwrap();
    let line = BufReader::new(child.stdout.take().unwrap()).lines().next().unwrap().unwrap();
    let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
    let tool = reply["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == "simplify-coverage").unwrap();
    assert!(tool["description"].as_str().unwrap().contains("--active-layer"));
    assert!(tool["description"].as_str().unwrap().contains("--preserve-boundary"));
    assert_eq!(tool["annotations"]["readOnlyHint"], false);
    drop(input);
    assert!(child.wait().unwrap().success());
}
