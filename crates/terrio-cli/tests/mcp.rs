//! `terrio mcp` over standard input and output, as an MCP client runs it.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

/// Sends `messages` to `terrio mcp`, one per line, and returns its replies by id, waiting for one
/// per request before closing its standard input (which stops the commands still running).
fn session(messages: &[Value]) -> HashMap<u64, Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_terrio")).arg("mcp").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for m in messages {
        writeln!(stdin, "{m}").unwrap();
    }
    let requests = messages.iter().filter(|m| m.get("id").is_some()).count();
    // Tool calls reply when their command is done, in any order.
    let replies: HashMap<u64, Value> = BufReader::new(child.stdout.take().unwrap())
        .lines()
        .take(requests)
        .map(|l| {
            let v: Value = serde_json::from_str(&l.unwrap()).unwrap();
            (v["id"].as_u64().unwrap(), v)
        })
        .collect();
    drop(stdin);
    assert!(child.wait().unwrap().success());
    replies
}

fn call(id: u32, name: &str, args: &[&str]) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": {"args": args}}})
}

#[test]
fn serves_the_commands() {
    let replies = session(&[
        json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        call(1, "formats", &["--json"]),
        call(2, "info", &["/no/such/file.tif"]),
        call(3, "info", &["--no-such-option"]),
    ]);
    assert_eq!(replies.len(), 4);
    assert_eq!(replies[&0]["result"]["protocolVersion"], "2025-06-18");

    let formats = &replies[&1]["result"];
    assert_eq!(formats["isError"], false);
    let list: Value = serde_json::from_str(formats["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(list.as_array().unwrap().iter().any(|f| f["name"] == "GTiff"));

    for (reply, code) in [(&replies[&2], "exit code 1"), (&replies[&3], "exit code 2")] {
        assert_eq!(reply["result"]["isError"], true);
        assert!(reply["result"]["content"][0]["text"].as_str().unwrap().contains(code), "{reply}");
    }
}

#[test]
fn topology_tools_are_discoverable_and_execute_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("point.geojson");
    let output = dir.path().join("buffer.gpkg");
    std::fs::write(&input,r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"name":"site"},"geometry":{"type":"Point","coordinates":[0,0]}}]}"#).unwrap();
    let replies = session(&[
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        call(2, "buffer", &[input.to_str().unwrap(), output.to_str().unwrap(), "--distance", "1"]),
    ]);
    let tools = replies[&1]["result"]["tools"].as_array().unwrap();
    for name in ["buffer", "overlay", "dissolve", "is-valid", "make-valid", "clean"] {
        assert!(tools.iter().any(|t| t["name"] == name), "missing {name}");
    }
    assert_eq!(replies[&2]["result"]["isError"], false);
    assert_eq!(terrio::open_vector(output.to_str().unwrap(), &Default::default()).unwrap().layers[0].len(), 1);
}

#[test]
fn viewshed_tool_writes_height_raster() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("ridge.tif");
    let output = dir.path().join("ground.tif");
    let cfg = terrio::RasterConfig { cols: 5, rows: 1, cell_size: 10.0, data_type: terrio::DataType::F64, ..Default::default() };
    let mut ds = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, vec![0.0, 0.0, 10.0, 0.0, 0.0]).unwrap(), false);
    ds.raster.crs.epsg = Some(32611);
    ds.set_geo_transform([0.0, 10.0, 0.0, 10.0, 0.0, -10.0]);
    terrio::write_raster_dataset(&ds, input.to_str().unwrap(), &Default::default()).unwrap();
    let replies = session(&[
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        call(2, "viewshed", &[input.to_str().unwrap(), output.to_str().unwrap(), "--ox", "5", "--oy", "5", "--om", "GROUND", "--cc", "0"]),
    ]);
    assert!(replies[&1]["result"]["tools"].as_array().unwrap().iter().any(|t| t["name"] == "viewshed"));
    assert_eq!(replies[&2]["result"]["isError"], false);
    let out = terrio::open_raster(output.to_str().unwrap()).unwrap();
    assert_eq!(out.raster.data_type, terrio::DataType::F64);
    assert_eq!(out.raster.get(0, 0, 3), 14.0);
    assert_eq!(out.raster.get(0, 0, 4), 18.0);
}

#[test]
fn fill_nodata_is_discoverable_and_executes() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("holes.asc");
    let output = dir.path().join("filled.tif");
    std::fs::write(&input, "ncols 3\nnrows 1\nxllcorner 0\nyllcorner 0\ncellsize 1\nNODATA_value -9999\n1 -9999 3\n").unwrap();
    let replies = session(&[
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        call(2, "fill-nodata", &[input.to_str().unwrap(), output.to_str().unwrap(), "--interpolation", "nearest"]),
    ]);
    assert!(replies[&1]["result"]["tools"].as_array().unwrap().iter().any(|t| t["name"] == "fill-nodata"));
    assert_eq!(replies[&2]["result"]["isError"], false);
    assert_eq!(terrio::open_raster(output.to_str().unwrap()).unwrap().raster.get(0, 0, 1), 1.0);
}

#[test]
fn pansharpen_tool_uses_all_spectral_bands() {
    let dir = tempfile::tempdir().unwrap();
    let pan_path = dir.path().join("pan.tif");
    let spectral_path = dir.path().join("spectral.tif");
    let output = dir.path().join("sharp.tif");
    for (path, bands, values) in [(&pan_path, 1, vec![80.0; 4]), (&spectral_path, 2, vec![20.0, 20.0, 20.0, 20.0, 40.0, 40.0, 40.0, 40.0])]
    {
        let cfg = terrio::RasterConfig { cols: 2, rows: 2, bands, cell_size: 1.0, data_type: terrio::DataType::U16, ..Default::default() };
        let mut ds = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, values).unwrap(), false);
        ds.raster.crs.epsg = Some(32611);
        ds.set_geo_transform([0.0, 1.0, 0.0, 2.0, 0.0, -1.0]);
        terrio::write_raster_dataset(&ds, path.to_str().unwrap(), &Default::default()).unwrap();
    }
    let replies = session(&[
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        call(2, "pansharpen", &[pan_path.to_str().unwrap(), spectral_path.to_str().unwrap(), output.to_str().unwrap()]),
    ]);
    assert!(replies[&1]["result"]["tools"].as_array().unwrap().iter().any(|t| t["name"] == "pansharpen"));
    assert_eq!(replies[&2]["result"]["isError"], false);
    let out = terrio::open_raster(output.to_str().unwrap()).unwrap();
    assert_eq!(out.raster.get(0, 0, 0), 53.0);
    assert_eq!(out.raster.get(1, 0, 0), 107.0);
}

#[test]
fn warp_serializes_and_reads_vrt_without_materializing_output() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("source.asc");
    let output = dir.path().join("warped.vrt");
    std::fs::write(&input, "ncols 3\nnrows 3\nxllcorner 0\nyllcorner 0\ncellsize 1\n1 2 3\n4 5 6\n7 8 9\n").unwrap();
    let replies = session(&[call(1, "warp", &[input.to_str().unwrap(), output.to_str().unwrap(), "--ts", "2", "2"])]);
    assert_eq!(replies[&1]["result"]["isError"], false, "{:?}", replies[&1]);
    assert!(std::fs::read_to_string(&output).unwrap().contains("VRTWarpedDataset"));
    let mut result = terrio::open_raster_lazy(output.to_str().unwrap()).unwrap();
    assert_eq!(result.read().unwrap().raster.data.to_f64_vec(), vec![1., 3., 7., 9.]);
}
