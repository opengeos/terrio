//! Coverage cleaning CLI behavior and automatic MCP discovery.
use std::process::{Command, Stdio};

#[test]
fn cleaning_retains_attributes_and_rejects_bad_options_before_output() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("input.geojson");
    std::fs::write(&source, r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"name":"west"},"geometry":{"type":"Polygon","coordinates":[[[0,0],[3,0],[3,2],[0,2],[0,0]]]}},{"type":"Feature","properties":{"name":"east"},"geometry":{"type":"Polygon","coordinates":[[[2,0],[4,0],[4,2],[2,2],[2,0]]]}}]}"#).unwrap();
    let run = |output: &str, options: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_terrio"))
            .current_dir(directory.path())
            .args(["clean-coverage", "input.geojson", output])
            .args(options)
            .output()
            .unwrap()
    };
    let result = run("clean.geojson", &["--merge-strategy", "min-area"]);
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let out = terrio::open_vector(directory.path().join("clean.geojson").to_str().unwrap(), &Default::default()).unwrap();
    assert_eq!(out.layers[0].len(), 2);
    assert!(terrio::ops::coverage::check_coverage(&out.layers[0], &Default::default()).unwrap().is_empty());
    assert_eq!(out.layers[0].features[0].attributes[0], terrio::FieldValue::Text("west".into()));
    assert!(!run("negative.geojson", &["--maximum-gap-width", "-1"]).status.success());
    assert!(!directory.path().join("negative.geojson").exists());
    assert!(!run("bad.geojson", &["--merge-strategy", "bad"]).status.success());
}

#[test]
fn cleaning_is_an_mcp_tool() {
    use std::io::{BufRead, BufReader, Write};
    let mut child = Command::new(env!("CARGO_BIN_EXE_terrio")).arg("mcp").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    writeln!(input, "{}", serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).unwrap();
    let line = BufReader::new(child.stdout.take().unwrap()).lines().next().unwrap().unwrap();
    let response: serde_json::Value = serde_json::from_str(&line).unwrap();
    let tool = response["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == "clean-coverage").unwrap();
    assert!(tool["description"].as_str().unwrap().contains("--merge-strategy"));
    assert_eq!(tool["annotations"]["readOnlyHint"], false);
    drop(input);
    assert!(child.wait().unwrap().success());
}

#[test]
fn cleaning_rejects_non_polygonal_inputs_without_creating_output() {
    let directory = tempfile::tempdir().unwrap();
    for (index, geometry) in [serde_json::Value::Null, serde_json::json!({"type":"Point","coordinates":[1,2]})].into_iter().enumerate() {
        let input = directory.path().join(format!("input-{index}.geojson"));
        let output = directory.path().join(format!("output-{index}.geojson"));
        std::fs::write(
            &input,
            serde_json::to_vec(
                &serde_json::json!({"type":"FeatureCollection","features":[{"type":"Feature","properties":{},"geometry":geometry}]}),
            )
            .unwrap(),
        )
        .unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_terrio")).arg("clean-coverage").arg(input).arg(&output).output().unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("polygonal geometry"));
        assert!(!output.exists());
    }
}
