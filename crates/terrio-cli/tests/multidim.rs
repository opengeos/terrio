//! Native scientific CLI commands and their MCP discovery/execution.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use terrio::{
    DataType,
    multidim::{Array, ArrayInfo, Dataset, Dimension, Group},
};

fn fixture(path: &std::path::Path) {
    mosaic_fixture(path, 0, 0);
}
fn mosaic_fixture(path: &std::path::Path, x_start: u16, offset: u16) {
    let dimensions: Vec<_> = [("time", 2), ("x", 3)]
        .into_iter()
        .map(|(name, size)| Dimension {
            name: name.into(),
            path: format!("/{name}"),
            size,
            unlimited: false,
            indexing_variable: Some(format!("/{name}")),
        })
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
        (offset..offset + 6).flat_map(u16::to_le_bytes).collect(),
    )
    .unwrap();
    let mut root = Group::new("/");
    root.dimensions = dimensions;
    root.arrays.push(a);
    for dimension in &root.dimensions {
        let values: Vec<u16> = if dimension.name == "x" { (x_start..x_start + 3).collect() } else { vec![0, 1] };
        root.arrays.push(
            Array::from_bytes(
                ArrayInfo {
                    name: dimension.name.clone(),
                    path: dimension.path.clone(),
                    data_type: DataType::U16,
                    dimensions: vec![dimension.clone()],
                    chunks: None,
                    attributes: json!({}),
                    coordinates: vec![],
                },
                values.into_iter().flat_map(u16::to_le_bytes).collect(),
            )
            .unwrap(),
        );
    }
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
fn positional_selections_and_json_cap_are_checked_before_reading() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cube.nc");
    fixture(&path);
    let run = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_terrio"))
            .args(["mdread", path.to_str().unwrap(), "--array", "/wind"])
            .args(extra)
            .output()
            .unwrap()
    };
    let capped = run(&["--max-elements", "2"]);
    assert!(!capped.status.success());
    assert!(String::from_utf8_lossy(&capped.stderr).contains("--slice/--isel"));
    assert!(run(&["--max-elements", "0"]).status.success());
    let selected = run(&["--isel-axis", "0=1", "--slice-axis", "0=0,2", "--max-elements", "2"]);
    assert!(selected.status.success());
    let json: Value = serde_json::from_slice(&selected.stdout).unwrap();
    assert_eq!(json["values"], json!([3, 4]));
}
#[test]
fn mcp_discovers_native_tools_and_reads_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cube.nc");
    fixture(&path);
    let mosaic = dir.path().join("mcp-mosaic.nc");
    let mut child = Command::new(env!("CARGO_BIN_EXE_terrio")).arg("mcp").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    for message in [
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"mdread","arguments":{"args":[path.to_str().unwrap(),"--array","/wind","--isel","time=1"]}}}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"mdmosaic","arguments":{"args":[path.to_str().unwrap(),path.to_str().unwrap(),mosaic.to_str().unwrap(),"--array","wind"]}}}),
    ] {
        writeln!(input, "{message}").unwrap();
    }
    let responses: Vec<Value> =
        BufReader::new(child.stdout.take().unwrap()).lines().take(3).map(|line| serde_json::from_str(&line.unwrap()).unwrap()).collect();
    drop(input);
    assert!(child.wait().unwrap().success());
    let list = responses.iter().find(|r| r["id"] == 1).unwrap();
    let tools = list["result"]["tools"].as_array().unwrap();
    for name in ["mdinfo", "mdread", "mdconvert", "mdmosaic"] {
        assert!(tools.iter().any(|t| t["name"] == name));
    }
    assert_eq!(tools.iter().find(|t| t["name"] == "mdread").unwrap()["annotations"]["readOnlyHint"], true);
    let read = responses.iter().find(|r| r["id"] == 2).unwrap();
    assert_eq!(read["result"]["isError"], false);
    let values: Value = serde_json::from_str(read["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(values["values"], json!([3, 4, 5]));
    assert_eq!(tools.iter().find(|t| t["name"] == "mdmosaic").unwrap()["annotations"]["readOnlyHint"], false);
    assert_eq!(responses.iter().find(|r| r["id"] == 3).unwrap()["result"]["isError"], false);
    let array = terrio::multidim::open(mosaic.to_str().unwrap()).unwrap().array("wind").unwrap();
    assert_eq!(array.read().unwrap().values().unwrap(), json!([0, 1, 2, 3, 4, 5]));
}

#[test]
fn named_transpose_rejects_repeated_axes_clearly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("repeated.nc");
    let dimension = Dimension { name: "x".into(), path: "/x".into(), size: 2, unlimited: false, indexing_variable: None };
    let mut root = Group::new("/");
    root.dimensions.push(dimension.clone());
    root.arrays.push(
        Array::from_bytes(
            ArrayInfo {
                name: "matrix".into(),
                path: "/matrix".into(),
                data_type: DataType::U8,
                dimensions: vec![dimension.clone(), dimension],
                chunks: None,
                attributes: json!({}),
                coordinates: vec![],
            },
            vec![0, 1, 2, 3],
        )
        .unwrap(),
    );
    Dataset { driver: "Memory".into(), root }.write(path.to_str().unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_terrio"))
        .args(["mdread", path.to_str().unwrap(), "--array", "matrix", "--transpose", "x,x"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("ambiguous transpose dimension 'x'"));
}

#[test]
fn mosaics_cli_tiles_and_preserves_existing_output_on_rejection() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.nc");
    let b = dir.path().join("b.nc");
    let output = dir.path().join("mosaic.nc");
    mosaic_fixture(&a, 0, 0);
    mosaic_fixture(&b, 3, 10);
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_terrio"))
            .args(["mdmosaic", a.to_str().unwrap(), b.to_str().unwrap(), output.to_str().unwrap(), "--array", "wind"])
            .output()
            .unwrap()
    };
    let result = run();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let array = terrio::multidim::open(output.to_str().unwrap()).unwrap().array("wind").unwrap();
    assert_eq!(array.shape(), [2, 6]);
    assert_eq!(array.read().unwrap().values().unwrap(), json!([0, 1, 2, 10, 11, 12, 3, 4, 5, 13, 14, 15]));
    let before = std::fs::read(&output).unwrap();
    assert!(!run().status.success());
    assert_eq!(std::fs::read(output).unwrap(), before);
}
