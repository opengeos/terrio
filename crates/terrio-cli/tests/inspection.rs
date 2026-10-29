//! Header identification and its read-only MCP surface.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn fixture(path: &std::path::Path) {
    let config = terrio::RasterConfig { rows: 2, cols: 2, data_type: terrio::DataType::U8, ..Default::default() };
    let source = terrio::RasterDataset::new(terrio::Raster::from_data(config, vec![1., 2., 3., 4.]).unwrap(), false);
    terrio::write_raster_dataset(&source, path.to_str().unwrap(), &Default::default()).unwrap();
}
#[test]
fn identifies_multiple_inputs_and_reports_unknowns() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("source.tif");
    fixture(&path);
    let unknown = temp.path().join("invalid.tif");
    std::fs::write(&unknown, b"not a dataset").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_terrio"))
        .args(["identify", "--json", "--report-failures", path.to_str().unwrap(), unknown.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let records: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(records[0]["driver"], "GTiff");
    assert!(records[1].get("driver").is_none());
    assert!(records[1]["error"].is_string());
}
#[test]
fn mcp_identification_and_check_are_read_only_and_execute() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("source.tif");
    fixture(&path);
    let mut child = Command::new(env!("CARGO_BIN_EXE_terrio")).arg("mcp").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    for message in [
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"identify","arguments":{"args":[path.to_str().unwrap(),"--json"]}}}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"check","arguments":{"args":[path.to_str().unwrap(),"--json"]}}}),
    ] {
        writeln!(input, "{message}").unwrap();
    }
    let replies: Vec<Value> =
        BufReader::new(child.stdout.take().unwrap()).lines().take(3).map(|line| serde_json::from_str(&line.unwrap()).unwrap()).collect();
    drop(input);
    assert!(child.wait().unwrap().success());
    let tools = replies.iter().find(|r| r["id"] == 1).unwrap()["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.iter().find(|t| t["name"] == "identify").unwrap()["annotations"]["readOnlyHint"], true);
    let reply = &replies.iter().find(|r| r["id"] == 2).unwrap()["result"];
    assert_eq!(reply["isError"], false);
    let records: Value = serde_json::from_str(reply["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(records[0]["driver"], "GTiff");
    assert_eq!(tools.iter().find(|t| t["name"] == "check").unwrap()["annotations"]["readOnlyHint"], true);
    let reply = &replies.iter().find(|r| r["id"] == 3).unwrap()["result"];
    assert_eq!(reply["isError"], false);
    let summary: Value = serde_json::from_str(reply["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(summary["raster_samples"], 4);
}

#[test]
fn check_reports_counts_and_nonzero_status_for_corruption() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("source.tif");
    fixture(&path);
    let output = Command::new(env!("CARGO_BIN_EXE_terrio")).args(["check", path.to_str().unwrap(), "--json"]).output().unwrap();
    assert!(output.status.success());
    let summary: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(summary["raster_samples"], 4);
    std::fs::write(&path, b"II*\0broken").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_terrio")).args(["check", path.to_str().unwrap(), "--json"]).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
}
