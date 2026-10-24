//! Native scientific CLI commands and their MCP discovery/execution.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use terrio::{
    DataType,
    multidim::{Array, ArrayInfo, Dataset, Dimension, Group},
};

fn fixture(path: &std::path::Path) {
    let dimensions: Vec<_> = [("time", 2), ("x", 3)]
        .into_iter()
        .map(|(name, size)| Dimension { name: name.into(), path: format!("/{name}"), size, unlimited: false, indexing_variable: None })
        .collect();
    let a = Array::from_bytes(
        ArrayInfo {
            name: "wind".into(),
            path: "/wind".into(),
            data_type: DataType::U16,
            dimensions: dimensions.clone(),
            chunks: Some(vec![1, 3]),
            attributes: json!({}),
            coordinates: vec![],
        },
        (0u16..6).flat_map(u16::to_le_bytes).collect(),
    )
    .unwrap();
    let mut root = Group::new("/");
    root.dimensions = dimensions;
    root.arrays.push(a);
    Dataset { driver: "Memory".into(), root }.write(path.to_str().unwrap()).unwrap();
}
#[test]
fn reads_a_named_hyperslab_as_json() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cube.nc");
    fixture(&path);
    let output = Command::new(env!("CARGO_BIN_EXE_terrio"))
        .args(["mdread", path.to_str().unwrap(), "--array", "/wind", "--isel", "time=1", "--slice", "x=0,2"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["shape"], json!([2]));
    assert_eq!(result["values"], json!([3, 4]));
    assert_eq!(result["data_type"], "UInt16");
}
#[test]
fn mcp_discovers_native_tools_and_reads_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cube.nc");
    fixture(&path);
    let mut child = Command::new(env!("CARGO_BIN_EXE_terrio")).arg("mcp").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    for message in [
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"mdread","arguments":{"args":[path.to_str().unwrap(),"--array","/wind","--isel","time=1"]}}}),
    ] {
        writeln!(input, "{message}").unwrap();
    }
    let responses: Vec<Value> =
        BufReader::new(child.stdout.take().unwrap()).lines().take(2).map(|line| serde_json::from_str(&line.unwrap()).unwrap()).collect();
    drop(input);
    assert!(child.wait().unwrap().success());
    let list = responses.iter().find(|r| r["id"] == 1).unwrap();
    let tools = list["result"]["tools"].as_array().unwrap();
    for name in ["mdinfo", "mdread", "mdconvert"] {
        assert!(tools.iter().any(|t| t["name"] == name));
    }
    assert_eq!(tools.iter().find(|t| t["name"] == "mdread").unwrap()["annotations"]["readOnlyHint"], true);
    let read = responses.iter().find(|r| r["id"] == 2).unwrap();
    assert_eq!(read["result"]["isError"], false);
    let values: Value = serde_json::from_str(read["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(values["values"], json!([3, 4, 5]));
}
